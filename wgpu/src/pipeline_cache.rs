//! Keep wgpu's pipeline cache between runs.
//!
//! Every render pipeline in this backend is built from scratch at start-up, with
//! `cache: None` on each descriptor, so the driver redoes the same work in every process on
//! every launch. A `wgpu::PipelineCache` is the Vulkan `VkPipelineCache` object: hand it the
//! blob a previous run produced and pipeline creation can skip straight to a finished object.
//!
//! This sits on top of whatever the driver already does for itself - Mesa keeps its own
//! shader cache in `~/.cache/mesa_shader_cache`, which covers the expensive half, compiling
//! shaders down to hardware instructions. What is left is pipeline object assembly, so
//! expect this to matter most on a first launch, after a driver update, and on drivers with
//! no cache of their own.
//!
//! `PIPELINE_CACHE` is a Vulkan-only feature and WMDE asks for Vulkan and nothing else, but
//! adapters may still refuse it, so everything here degrades to `None` and the pipelines are
//! built the way they always were. `WMDE_PIPELINE_CACHE=0` turns it off outright.

use std::path::{Path, PathBuf};

/// A pipeline cache and the file it came from.
#[derive(Debug)]
pub struct Cache {
    raw: wgpu::PipelineCache,
    path: PathBuf,
    /// What was on disk when this was loaded, so an unchanged cache is not rewritten.
    loaded: Option<Vec<u8>>,
}

impl Cache {
    /// The cache to hand to a `RenderPipelineDescriptor`.
    pub fn raw(&self) -> &wgpu::PipelineCache {
        &self.raw
    }

    /// Writes the cache back out.
    ///
    /// Worth calling once, after every pipeline has been built - the blob only grows as
    /// pipelines are added to it, so saving earlier just saves less.
    pub fn store(&self) {
        let Some(data) = self.raw.get_data() else {
            return;
        };

        // Every application in the session builds the same pipelines from the same blob, so
        // after the first run there is usually nothing new to record. Writing anyway would
        // have a dozen processes rewriting an identical file at every login.
        if self.loaded.as_deref() == Some(data.as_slice()) {
            return;
        }

        if let Err(error) = write_atomically(&self.path, &data) {
            // A cache that cannot be written is a slow start, not a broken one.
            log::warn!(
                "could not save the pipeline cache to {}: {error}",
                self.path.display()
            );
        }
    }
}

/// Loads the pipeline cache belonging to `adapter`, if this device supports one.
pub fn load(device: &wgpu::Device, adapter: &wgpu::Adapter) -> Option<Cache> {
    if std::env::var_os("WMDE_PIPELINE_CACHE").is_some_and(|v| v == "0") {
        return None;
    }

    if !device.features().contains(wgpu::Features::PIPELINE_CACHE) {
        return None;
    }

    let path = cache_path(adapter)?;
    let data = std::fs::read(&path).ok();

    log::debug!(
        "pipeline cache {} ({})",
        path.display(),
        match &data {
            Some(data) => format!("{} bytes", data.len()),
            None => "new".to_owned(),
        }
    );

    // SAFETY: wgpu cannot prove that a blob handed to it came from `PipelineCache::get_data`
    // on a compatible device, and neither can we - the file could have been edited or
    // written by another machine. `fallback: true` is the mitigation the API documents:
    // data the driver does not recognise is discarded and an empty cache is returned
    // instead of the call failing. The file also lives in this user's own cache directory
    // and is named after the adapter and driver that produced it, so a mismatch takes
    // deliberate effort.
    let raw = unsafe {
        device.create_pipeline_cache(&wgpu::PipelineCacheDescriptor {
            label: Some("iced_wgpu::pipeline_cache"),
            data: data.as_deref(),
            fallback: true,
        })
    };

    Some(Cache {
        raw,
        path,
        loaded: data,
    })
}

/// Where this adapter's blob lives.
///
/// The name carries the adapter and driver rather than a hash of them: a cache keyed by
/// something unreadable is a cache nobody can reason about when it misbehaves. Changing
/// GPU, driver or iced_wgpu version simply selects a different file, and the stale one is
/// left behind rather than fed to a driver that never wrote it.
fn cache_path(adapter: &wgpu::Adapter) -> Option<PathBuf> {
    let dir = cache_dir()?.join("wmde/wgpu-pipeline");

    if let Err(error) = std::fs::create_dir_all(&dir) {
        log::warn!("could not create {}: {error}", dir.display());
        return None;
    }

    let info = adapter.get_info();
    let name = format!(
        "{}-{}-{:04x}-{:04x}-{}-{}.bin",
        sanitize(&format!("{:?}", info.backend)),
        sanitize(&info.name),
        info.vendor,
        info.device,
        sanitize(&info.driver_info),
        env!("CARGO_PKG_VERSION"),
    );

    Some(dir.join(name))
}

fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }

    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache"))
}

/// Anything that is not plainly safe in a file name becomes `_`, and the result is capped -
/// `driver_info` is free-form vendor text and has no length anyone promised.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(40)
        .collect()
}

/// Write through a temporary file and rename.
///
/// Several WMDE processes start at once at login and they all write the same path. A
/// half-written blob is exactly the input `fallback: true` has to rescue, and a rename is
/// atomic, so a reader either sees the previous cache or the new one and never a torn file.
fn write_atomically(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));

    std::fs::write(&tmp, data)?;

    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }

    Ok(())
}
