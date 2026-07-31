//! Build the system font index across all cores.
//!
//! `cosmic_text::FontSystem::new_with_fonts` calls `fontdb::Database::load_system_fonts`,
//! which walks every font directory and calls `load_font_file` on each file in turn: one
//! `mmap` plus one `name`-table parse per face, strictly serially. On a desktop with a few
//! hundred installed fonts that is the single most expensive step of application start-up -
//! cosmic-text's own documentation puts `FontSystem::new` at "up to a second" - and it is
//! pure CPU work with no ordering between files.
//!
//! This module does the same job in parallel. Directory discovery is a faithful copy of
//! fontdb's, including its fontconfig pass, so the set of files considered is identical and
//! nothing drops out of the index; only the parsing is spread across threads. Each thread
//! fills a private `Database` and the faces are merged with `push_face_info`, which is the
//! same call fontdb makes internally for every face it parses.
//!
//! Two escape hatches, because a font that fails to load renders as tofu and that is worse
//! than a slow start: `WMDE_FONT_PARALLEL=0` forces fontdb's own loader, and discovery that
//! turns up no files at all falls back to it automatically.

use cosmic_text::fontdb;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Builds a [`cosmic_text::FontSystem`] holding the system fonts plus `extra`.
///
/// Mirrors `FontSystem::new_with_fonts` step for step - system fonts first, then the
/// caller's sources, then the three generic-family defaults - so the only difference from
/// stock cosmic-text is how long the first step takes.
pub fn font_system(
    extra: impl IntoIterator<Item = fontdb::Source>,
) -> cosmic_text::FontSystem {
    let mut db = fontdb::Database::new();

    let started = std::time::Instant::now();

    load_system_fonts(&mut db);

    for source in extra {
        let _ = db.load_font_source(source);
    }

    // cosmic-text logs the same line from its own loader, so `RUST_LOG=debug` compares the
    // two directly - and it is the only measurement of this change that does not need a
    // stopwatch on the whole application.
    log::debug!(
        "Parsed {} font faces in {}ms.",
        db.len(),
        started.elapsed().as_millis()
    );

    // cosmic-text sets these in `new_with_fonts` after loading, overriding whatever the
    // fontconfig pass picked. Repeat them verbatim: they decide what `Family::Monospace`
    // and friends resolve to, and getting them wrong changes the font on screen.
    db.set_monospace_family("Noto Sans Mono");
    db.set_sans_serif_family("Open Sans");
    db.set_serif_family("DejaVu Serif");

    cosmic_text::FontSystem::new_with_locale_and_db(locale(), db)
}

/// The locale cosmic-text would have picked for itself.
fn locale() -> String {
    sys_locale::get_locale().unwrap_or_else(|| {
        log::warn!("failed to get system locale, falling back to en-US");
        String::from("en-US")
    })
}

/// Everywhere except Linux, discovery rules differ (registry hives, `/System/Library`,
/// asset bundles) and WMDE does not run there. Parsing in parallel is not worth carrying a
/// second copy of someone else's platform logic, so hand those targets straight back.
#[cfg(not(all(unix, not(target_os = "macos"), not(target_os = "android"))))]
fn load_system_fonts(db: &mut fontdb::Database) {
    db.load_system_fonts();
}

#[cfg(all(unix, not(target_os = "macos"), not(target_os = "android")))]
fn load_system_fonts(db: &mut fontdb::Database) {
    if std::env::var_os("WMDE_FONT_PARALLEL").is_some_and(|v| v == "0") {
        db.load_system_fonts();
        return;
    }

    let files = collect_files(&font_dirs());

    if files.is_empty() {
        // Our discovery disagreed with fontdb's. Whatever the reason, an empty font
        // database means every string on screen becomes tofu, so hand the job back.
        log::warn!("no font files found; falling back to fontdb's own loader");
        db.load_system_fonts();
        return;
    }

    parse_in_parallel(db, &files);
}

/// The directories fontdb would scan, in the same order and by the same rules.
fn font_dirs() -> Vec<PathBuf> {
    let home = std::env::var("HOME");

    if let Some(dirs) = fontconfig_dirs(&home) {
        return dirs;
    }

    // fontdb's `load_no_fontconfig`, reached when fontconfig lists no directories.
    let mut dirs = vec![
        PathBuf::from("/usr/share/fonts/"),
        PathBuf::from("/usr/local/share/fonts/"),
    ];
    if let Ok(home) = &home {
        dirs.push(Path::new(home).join(".fonts"));
        dirs.push(Path::new(home).join(".local/share/fonts"));
    }
    dirs
}

/// fontdb's `load_fontconfig`, minus the alias handling - cosmic-text overwrites the
/// generic families right afterwards anyway, so parsing them here would be dead work.
fn fontconfig_dirs(home: &Result<String, std::env::VarError>) -> Option<Vec<PathBuf>> {
    let mut fontconfig = fontconfig_parser::FontConfig::default();

    if let Ok(config_file) = std::env::var("FONTCONFIG_FILE") {
        let _ = fontconfig.merge_config(Path::new(&config_file));
    } else {
        let xdg_config_home = if let Ok(val) = std::env::var("XDG_CONFIG_HOME") {
            Some(PathBuf::from(val))
        } else {
            home.as_ref().ok().map(|h| Path::new(h).join(".config"))
        };

        let read_global = match xdg_config_home {
            Some(p) => fontconfig
                .merge_config(&p.join("fontconfig/fonts.conf"))
                .is_err(),
            None => true,
        };

        if read_global {
            let _ = fontconfig.merge_config(Path::new("/etc/fonts/local.conf"));
        }
        let _ = fontconfig.merge_config(Path::new("/etc/fonts/fonts.conf"));
    }

    if fontconfig.dirs.is_empty() {
        return None;
    }

    Some(
        fontconfig
            .dirs
            .into_iter()
            .filter_map(|dir| {
                // Path::strip_prefix compares components and reports a Result, unlike the
                // str method of the same name. Leaving the path alone first also keeps the
                // borrow out of the way of moving it.
                if !dir.path.starts_with("~") {
                    return Some(dir.path);
                }

                let rest = dir.path.strip_prefix("~").ok()?.to_path_buf();

                home.as_ref().ok().map(|h| Path::new(h).join(rest))
            })
            .collect(),
    )
}

/// Every font file under `dirs`, deduplicated.
///
/// Only `stat` and `readdir` happen here; the expensive part is the parse that follows.
/// Paths are canonicalised so a symlinked directory cannot send the walk round in circles -
/// fontdb guards the same case, and the resulting duplicate faces are worthless anyway.
fn collect_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();

    for dir in dirs {
        walk(dir, &mut files, &mut seen);
    }

    files
}

fn walk(dir: &Path, files: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in entries.flatten() {
        let Ok(path) = entry.path().canonicalize() else {
            continue;
        };
        if !seen.insert(path.clone()) {
            continue;
        }

        // `entry.file_type()` reports the link itself; ask about the target instead, so a
        // symlinked font or font directory is followed exactly as fontdb follows it.
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };

        if meta.is_dir() {
            walk(&path, files, seen);
        } else if meta.is_file() && is_font(&path) {
            files.push(path);
        }
    }
}

fn is_font(path: &Path) -> bool {
    // The exact extension set fontdb accepts, both cases, and nothing else.
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some(
            "ttf" | "ttc" | "otf" | "otc" | "TTF" | "TTC" | "OTF" | "OTC"
        )
    )
}

fn parse_in_parallel(db: &mut fontdb::Database, files: &[PathBuf]) {
    // Past a handful of threads this stops being CPU-bound and turns into page faults on
    // freshly mapped font files, so the extra threads buy nothing.
    let threads = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(files.len())
        .min(8);

    if threads <= 1 {
        for path in files {
            let _ = db.load_font_file(path);
        }
        return;
    }

    let chunk = files.len().div_ceil(threads);

    let parsed: Vec<fontdb::Database> = std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|chunk| {
                scope.spawn(move || {
                    let mut local = fontdb::Database::new();
                    for path in chunk {
                        // fontdb logs and skips unreadable files; match that, a broken
                        // font in the system directory must not take the app down.
                        let _ = local.load_font_file(path);
                    }
                    local
                })
            })
            .collect();

        handles
            .into_iter()
            .filter_map(|handle| match handle.join() {
                Ok(local) => Some(local),
                Err(_) => {
                    log::warn!("a font-parsing thread panicked; its fonts are missing");
                    None
                }
            })
            .collect()
    });

    for local in parsed {
        for face in local.faces() {
            // The same call fontdb makes for every face it parses. `push_face_info`
            // reassigns the id, so the dummy ids from the private databases never leak.
            let _ = db.push_face_info(face.clone());
        }
    }
}
