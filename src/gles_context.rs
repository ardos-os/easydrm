use std::{ffi::CString, ptr::NonNull};

use gbm::{AsRaw, Device as GbmDevice};
use glutin::api::egl;
use glutin::config::ConfigTemplateBuilder;
use glutin::context::ContextAttributesBuilder;
use glutin::prelude::*;
use raw_window_handle::{GbmDisplayHandle, RawDisplayHandle};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GlesContextError {
    #[error("Failed to create EGL display")]
    DisplayCreationFailed,
    #[error("No suitable EGL config found")]
    NoConfigFound,
    #[error("Failed to create EGL context: {0}")]
    EglContextCreationFailed(String),
    #[error("Failed to make context current")]
    MakeCurrentFailed,
}

pub struct GlesContext {
    display: egl::display::Display,
    _config: egl::config::Config,
    context: egl::context::PossiblyCurrentContext,
    gl: crate::gl::Gles2,
}

impl GlesContext {
    /// Creates a shared OpenGL ES context that can be reused across monitor surfaces.
    pub fn new(gbm_device: &GbmDevice<std::fs::File>) -> Result<Self, GlesContextError> {
        // Create EGL display from GBM device
        let raw_display_handle = RawDisplayHandle::Gbm(GbmDisplayHandle::new(
            NonNull::new(gbm_device.as_raw() as *mut std::ffi::c_void)
                .ok_or(GlesContextError::DisplayCreationFailed)?,
        ));

        let display = unsafe { egl::display::Display::new(raw_display_handle) }
            .map_err(|_| GlesContextError::DisplayCreationFailed)?;

        // Find best EGL config
        let config = find_egl_config(&display)?;

        // Create a surfaceless shared EGL context.
        let context = unsafe {
            display
                .create_context(&config, &ContextAttributesBuilder::new().build(None))
                .map_err(|e| GlesContextError::EglContextCreationFailed(e.to_string()))?
                .make_current_surfaceless()
                .map_err(|_| GlesContextError::MakeCurrentFailed)?
        };

        // Load OpenGL function pointers
        let gl = crate::gl::Gles2::load_with(|symbol| {
            let c_symbol = CString::new(symbol).unwrap();
            display.get_proc_address(&c_symbol)
        });

        Ok(GlesContext {
            display,
            _config: config,
            context,
            gl,
        })
    }

    /// Makes the shared context current without a draw/read surface.
    pub fn make_current_surfaceless(&self) -> Result<(), GlesContextError> {
        self.context
            .make_current_surfaceless()
            .map_err(|_| GlesContextError::MakeCurrentFailed)?;
        Ok(())
    }

    /// Gets a function pointer for loading OpenGL functions
    pub fn get_proc_address(&self, symbol: &str) -> *const std::ffi::c_void {
        let c_symbol = CString::new(symbol).unwrap();
        self.display.get_proc_address(&c_symbol)
    }

    /// Gets a reference to the OpenGL ES bindings
    pub fn gl(&self) -> &crate::gl::Gles2 {
        &self.gl
    }

    /// Gets a reference to the EGL display for advanced operations (like fences)
    pub(crate) fn display(&self) -> &egl::display::Display {
        &self.display
    }
}

// GlesContext uses RAII - all fields are automatically dropped

/// Finds the best EGL config with the highest number of samples
fn find_egl_config(
    display: &egl::display::Display,
) -> Result<egl::config::Config, GlesContextError> {
    unsafe { display.find_configs(ConfigTemplateBuilder::new().build()) }
        .map_err(|_| GlesContextError::NoConfigFound)?
        .reduce(|config, acc| {
            if config.num_samples() > acc.num_samples() {
                config
            } else {
                acc
            }
        })
        .ok_or(GlesContextError::NoConfigFound)
}
