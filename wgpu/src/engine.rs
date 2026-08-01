use crate::graphics::{Antialiasing, Shell};
use crate::primitive;
use crate::quad;
use crate::text;
use crate::triangle;

use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub struct Engine {
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) format: wgpu::TextureFormat,

    pub(crate) quad_pipeline: quad::Pipeline,
    pub(crate) text_pipeline: text::Pipeline,
    pub(crate) triangle_pipeline: triangle::Pipeline,
    #[cfg(any(feature = "image", feature = "svg"))]
    pub(crate) image_pipeline: crate::image::Pipeline,
    pub(crate) primitive_storage: Arc<RwLock<primitive::Storage>>,
    _shell: Shell,
}

impl Engine {
    pub fn new(
        adapter: &wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        format: wgpu::TextureFormat,
        antialiasing: Option<Antialiasing>, // TODO: Initialize AA pipelines lazily
        shell: Shell,
    ) -> Self {
        // WMDE: every pipeline below is built here, once per process, and without a cache
        // the driver reassembles all of them on every launch. Load the blob the last run
        // left behind, hand it to each descriptor, and write it back once they all exist.
        // `None` whenever the adapter has no PIPELINE_CACHE - the pipelines are then built
        // exactly as they were before. See pipeline_cache for what this does and does not
        // cover.
        let cache = crate::pipeline_cache::load(&device, adapter);
        let cache_ref = cache.as_ref().map(crate::pipeline_cache::Cache::raw);

        let engine = Self {
            format,

            quad_pipeline: quad::Pipeline::new(&device, format, cache_ref),
            text_pipeline: text::Pipeline::new(&device, &queue, format),
            triangle_pipeline: triangle::Pipeline::new(
                &device,
                format,
                antialiasing,
                cache_ref,
            ),

            #[cfg(any(feature = "image", feature = "svg"))]
            image_pipeline: {
                let backend = adapter.get_info().backend;

                crate::image::Pipeline::new(
                    &device, format, backend, cache_ref,
                )
            },

            primitive_storage: Arc::new(RwLock::new(
                primitive::Storage::default(),
            )),

            device,
            queue,
            _shell: shell,
        };

        // Saved after the fact rather than on drop: the blob only grows as pipelines are
        // added to it, and a process that is killed never runs a destructor anyway.
        if let Some(cache) = &cache {
            cache.store();
        }

        engine
    }

    #[cfg(any(feature = "image", feature = "svg"))]
    pub fn create_image_cache(&self) -> crate::image::Cache {
        self.image_pipeline.create_cache(
            &self.device,
            &self.queue,
            &self._shell,
        )
    }

    pub fn trim(&mut self) {
        self.text_pipeline.trim();

        self.primitive_storage
            .write()
            .expect("primitive storage should be writable")
            .trim();
    }
}
