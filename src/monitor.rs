use std::{collections::HashMap, hash::Hash};

use drm::control::{self, atomic::AtomicModeReq, connector, crtc, plane, property};
use thiserror::Error;

use crate::MonitorContextCreationRequest;
use crate::gles_context::{GlesContext, GlesContextError};

/// DRM resources dedicated to a monitor instance.
pub(crate) struct MonitorResourceAllocation {
    pub crtc_info: crtc::Info,
    pub primary_plane: plane::Handle,
    pub cursor_plane: Option<plane::Handle>,
}

/// Represents a connected display monitor with its own OpenGL ES rendering context.
///
/// Each monitor manages:
/// - DRM resources (connector, CRTC, planes)
/// - Display mode configuration with 3-state tracking
/// - Independent OpenGL ES context
/// - Render state tracking
///
/// # Display Modes (3-State System)
///
/// - **default_mode**: The optimal mode for this monitor (highest resolution + refresh rate)
///   - e.g., 4K@120Hz for a high-end monitor
///   - This is the fallback when no custom mode is requested
/// - **requested_mode**: The mode the user wants to use
///   - `None` means use the optimal default mode
///   - `Some(mode)` means use a custom mode (e.g., 1080p@60Hz)
/// - **current_mode**: The mode currently set in hardware
///   - `None` if no mode has been set yet (new monitor) or TTY lost focus
///   - `Some(mode)` is the last mode successfully committed to DRM
///   - Compared against `requested_mode` each frame to trigger mode sets
///
/// # Rendering Flow
///
/// ```ignore
/// if monitor.can_render() {
///     monitor.make_current()?;
///     // OpenGL rendering calls here...
///     gl::Clear(gl::COLOR_BUFFER_BIT);
/// }
/// // EasyDRM::swap_buffers() will orchestrate the monitor swaps
/// ```
pub struct Monitor<T> {
    connector_id: connector::Handle,
    current_crtc: crtc::Info,
    default_mode: control::Mode,
    requested_mode: Option<control::Mode>,
    current_mode: Option<control::Mode>,
    primary_plane_id: plane::Handle,
    cursor_plane_id: Option<plane::Handle>,
    gles_context: GlesContext,
    can_render: bool,
    was_drawn: bool,
    // DRM state tracking
    previous_bo: Option<gbm::BufferObject<()>>,
    previous_fence_fd: Option<i32>,
    previous_sync: Option<*mut std::ffi::c_void>,
    connector_properties: HashMap<String, property::Info>,
    crtc_properties: HashMap<String, property::Info>,
    plane_properties: HashMap<String, property::Info>,
    first_frame: bool,
    // User context
    user_context: T,
}

impl<T> PartialEq for Monitor<T> {
    fn eq(&self, other: &Self) -> bool {
        other.connector_id == self.connector_id
    }
}

impl<T> Eq for Monitor<T> {}

impl<T> Hash for Monitor<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.connector_id.hash(state);
    }
}

/// Errors that can occur during monitor initialization
#[derive(Debug, Error)]
pub enum MonitorSetupError {
    #[error("IO Error: {0}")]
    IOError(#[from] std::io::Error),
    #[error("monitor is not connected")]
    NotConnected,
    #[error("monitor doesn't have any crtcs available")]
    NoCRTCFound,
    #[error("monitor doesn't have any display modes")]
    NoModesFound,
    #[error("no primary plane found for this monitor")]
    NoPrimaryPlaneFound,
    #[error("failed to create OpenGL ES context: {0}")]
    GlesContextError(#[from] GlesContextError),
    #[error("DRM error: {0}")]
    DrmError(String),
}

impl<T> Monitor<T> {
    pub(crate) fn setup<F>(
        card: &impl control::Device,
        gbm_device: &gbm::Device<std::fs::File>,
        connector_id: connector::Handle,
        allocation: MonitorResourceAllocation,
        context_constructor: F,
    ) -> Result<Self, MonitorSetupError>
    where
        F: for<'a> FnOnce(&MonitorContextCreationRequest<'a>) -> T,
    {
        let connector = card.get_connector(connector_id, true)?;
        let MonitorResourceAllocation {
            crtc_info,
            primary_plane,
            cursor_plane,
        } = allocation;
        // Get the optimal/preferred mode (highest resolution + refresh rate)
        let default_mode = connector
            .modes()
            .first()
            .cloned()
            .ok_or(MonitorSetupError::NoModesFound)?;

        // Create the OpenGL ES context for this monitor
        let gles_context = GlesContext::new(gbm_device, &default_mode)?;

        // Initialize user context with access to GL bindings
        let get_proc_address = |symbol: &str| gles_context.get_proc_address(symbol);
        let request = MonitorContextCreationRequest {
            gl: gles_context.gl(),
            width: default_mode.size().0 as _,
            height: default_mode.size().1 as _,
            get_proc_address: &get_proc_address,
        };
        let user_context = context_constructor(&request);

        // Cache DRM properties for atomic commits
        let connector_properties = card
            .get_properties(connector_id)?
            .as_hashmap(card)
            .map_err(|e| {
                MonitorSetupError::DrmError(format!("Failed to get connector properties: {}", e))
            })?;

        let crtc_properties = card
            .get_properties(crtc_info.handle())?
            .as_hashmap(card)
            .map_err(|e| {
                MonitorSetupError::DrmError(format!("Failed to get CRTC properties: {}", e))
            })?;

        let plane_properties = card
            .get_properties(primary_plane)?
            .as_hashmap(card)
            .map_err(|e| {
                MonitorSetupError::DrmError(format!("Failed to get plane properties: {}", e))
            })?;

        Ok(Monitor {
            connector_id,
            current_crtc: crtc_info,
            default_mode,
            requested_mode: None, // Use default mode initially
            current_mode: None,   // No mode set in hardware yet
            primary_plane_id: primary_plane,
            cursor_plane_id: cursor_plane,
            gles_context,
            can_render: true, // Initially ready to render
            was_drawn: false,
            previous_bo: None,
            previous_fence_fd: None,
            previous_sync: None,
            connector_properties,
            crtc_properties,
            plane_properties,
            first_frame: true,
            user_context,
        })
    }

    /// Get a reference to the user context
    pub fn context(&self) -> &T {
        &self.user_context
    }

    /// Get a mutable reference to the user context
    pub fn context_mut(&mut self) -> &mut T {
        &mut self.user_context
    }

    /// Returns whether this monitor is ready to be rendered to.
    ///
    /// This is updated by `poll_events()` based on page flip completion and VBlank timing.
    /// Only render to monitors where this returns `true` to avoid frame drops.
    pub fn can_render(&self) -> bool {
        self.can_render
    }

    /// Internal flag indicating if this monitor was drawn to this frame.
    ///
    /// Used by `EasyDRM::swap_buffers()` to determine which monitors need presentation.
    pub(crate) fn was_drawn(&self) -> bool {
        self.was_drawn
    }

    /// Sets the can_render flag (used by poll_events).
    pub(crate) fn set_can_render(&mut self, value: bool) {
        self.can_render = value;
    }

    /// Resets the was_drawn flag for the next frame (used by EasyDRM::swap_buffers).
    pub(crate) fn reset_drawn_flag(&mut self) {
        self.was_drawn = false;
    }

    /// Makes this monitor's OpenGL context current for rendering.
    ///
    /// Call this before any OpenGL rendering operations. Automatically marks
    /// this monitor as having been drawn to for this frame.
    ///
    /// # Errors
    ///
    /// Returns an error if the EGL context cannot be made current.
    ///
    /// # Example
    ///
    /// ```ignore
    /// if monitor.can_render() {
    ///     monitor.make_current()?;
    ///     gl::ClearColor(0.0, 0.0, 0.0, 1.0);
    ///     gl::Clear(gl::COLOR_BUFFER_BIT);
    /// }
    /// ```
    pub fn make_current(&mut self) -> Result<(), GlesContextError> {
        self.gles_context.make_current()?;
        self.was_drawn = true;
        Ok(())
    }

    /// Swaps buffers and submits an atomic commit to display the rendered content.
    ///
    /// This handles:
    /// - EGL buffer swap
    /// - Atomic DRM commit with the new framebuffer
    /// - Mode setting if `requested_mode != current_mode`
    ///
    /// **Warning:** This is internal API only. Must be called by `EasyDRM::swap_buffers()`
    /// with proper timing to avoid tearing or frame drops. Calling this directly can
    /// cause synchronization issues.
    pub(crate) fn swap_buffers(
        &mut self,
        card: &impl control::Device,
        atomic_req: &mut AtomicModeReq,
    ) -> Result<(), MonitorSetupError> {
        self.gles_context.make_current()?;
        // Flush GL and swap EGL buffers
        // Note: GL functions need to be loaded first with gl::load_with()
        // This will be called by the user before rendering

        // Get the new buffer object from GBM
        let bo = self
            .gles_context
            .swap_buffers()
            .map_err(|e| MonitorSetupError::DrmError(format!("Failed to swap buffers: {}", e)))?;

        // Cleanup previous fence and sync object
        if let Some(old_fence_fd) = self.previous_fence_fd.take() {
            unsafe {
                libc::close(old_fence_fd);
            }
        }
        if let Some(old_sync) = self.previous_sync.take() {
            self.destroy_egl_sync(old_sync);
        }

        // Create EGL fence for GPU->DRM synchronization
        let (fence_fd, sync) = self
            .create_egl_fence()
            .map_err(|e| MonitorSetupError::DrmError(format!("Failed to create fence: {}", e)))?;

        // Create DRM framebuffer from the buffer object
        // NOTE: Framebuffer will drop automatically after atomic commit (RAII)
        // We only need to keep the buffer object alive for proper double-buffering
        let fb = card.add_framebuffer(&bo, 24, 32).map_err(|e| {
            MonitorSetupError::DrmError(format!("Failed to add framebuffer: {}", e))
        })?;

        // Build atomic commit request

        // Determine which mode to use
        let target_mode = self.requested_mode.as_ref().unwrap_or(&self.default_mode);
        let needs_mode_set = self.needs_mode_set();

        // Set connector CRTC_ID
        atomic_req.add_property(
            self.connector_id,
            self.connector_properties["CRTC_ID"].handle(),
            property::Value::CRTC(Some(self.current_crtc.handle())),
        );
        atomic_req.add_property(
            self.primary_plane_id,
            self.plane_properties["CRTC_ID"].handle(),
            property::Value::CRTC(Some(self.current_crtc.handle())),
        );

        // Configure plane for full-screen scanout
        let (width, height) = target_mode.size();

        // Source rectangle (in 16.16 fixed point)
        atomic_req.add_property(
            self.primary_plane_id,
            self.plane_properties["SRC_X"].handle(),
            property::Value::UnsignedRange(0),
        );
        atomic_req.add_property(
            self.primary_plane_id,
            self.plane_properties["SRC_Y"].handle(),
            property::Value::UnsignedRange(0),
        );
        atomic_req.add_property(
            self.primary_plane_id,
            self.plane_properties["SRC_W"].handle(),
            property::Value::UnsignedRange((width as u64) << 16),
        );
        atomic_req.add_property(
            self.primary_plane_id,
            self.plane_properties["SRC_H"].handle(),
            property::Value::UnsignedRange((height as u64) << 16),
        );

        // Destination rectangle
        atomic_req.add_property(
            self.primary_plane_id,
            self.plane_properties["CRTC_X"].handle(),
            property::Value::SignedRange(0),
        );
        atomic_req.add_property(
            self.primary_plane_id,
            self.plane_properties["CRTC_Y"].handle(),
            property::Value::SignedRange(0),
        );
        atomic_req.add_property(
            self.primary_plane_id,
            self.plane_properties["CRTC_W"].handle(),
            property::Value::UnsignedRange(width as u64),
        );
        atomic_req.add_property(
            self.primary_plane_id,
            self.plane_properties["CRTC_H"].handle(),
            property::Value::UnsignedRange(height as u64),
        ); // If mode set is needed (first frame or mode change)
        if needs_mode_set {
            // Create mode blob and set MODE_ID
            let mode_blob = card.create_property_blob(target_mode).map_err(|e| {
                MonitorSetupError::DrmError(format!("Failed to create mode blob: {}", e))
            })?;
            atomic_req.add_property(
                self.current_crtc.handle(),
                self.crtc_properties["MODE_ID"].handle(),
                mode_blob,
            );

            // Set CRTC active
            atomic_req.add_property(
                self.current_crtc.handle(),
                self.crtc_properties["ACTIVE"].handle(),
                property::Value::Boolean(true),
            );
        }

        // Set framebuffer on plane (always needed)
        atomic_req.add_property(
            self.primary_plane_id,
            self.plane_properties["FB_ID"].handle(),
            property::Value::Framebuffer(Some(fb)),
        );

        // Add fence for synchronization (prefer CRTC, fallback to plane)
        if let Some(fence_prop) = self.crtc_properties.get("IN_FENCE_FD") {
            atomic_req.add_property(
                self.current_crtc.handle(),
                fence_prop.handle(),
                property::Value::SignedRange(fence_fd as i64),
            );
        } else if let Some(fence_prop) = self.plane_properties.get("IN_FENCE_FD") {
            atomic_req.add_property(
                self.primary_plane_id,
                fence_prop.handle(),
                property::Value::SignedRange(fence_fd as i64),
            );
        }

        // Store buffer object, fence, and sync for next frame
        // Buffer must stay alive until after next lock_front_buffer (double-buffering)
        // Fence/sync cleaned up on next swap_buffers or Drop
        self.previous_bo = Some(bo);
        self.previous_fence_fd = Some(fence_fd);
        self.previous_sync = Some(sync);

        // Update state
        self.first_frame = false;
        self.can_render = false; // Wait for page flip event

        // Mark mode as set if we just did a mode set
        if needs_mode_set {
            self.mark_mode_set();
        }

        Ok(())
    }

    /// Creates an EGL fence for GPU->DRM synchronization
    /// Returns (fence_fd, sync_object)
    fn create_egl_fence(&self) -> Result<(i32, *mut std::ffi::c_void), String> {
        unsafe {
            // Get EGL bindings from glutin display
            let egl = self.gles_context.display().egl();

            const EGL_SYNC_FENCE_KHR: u32 = 0x3144;

            // Create EGL sync fence
            let sync = egl.CreateSyncKHR(
                egl.GetCurrentDisplay(),
                EGL_SYNC_FENCE_KHR,
                std::ptr::null(),
            );

            if sync.is_null() {
                return Err("Failed to create EGL sync".to_string());
            }

            // Duplicate as a native fence FD for DRM
            let fence_fd = egl.DupNativeFenceFDANDROID(egl.GetCurrentDisplay(), sync);

            if fence_fd < 0 {
                return Err(format!("Failed to duplicate fence FD: fence_fd={fence_fd}"));
            }

            Ok((fence_fd, sync as *mut std::ffi::c_void))
        }
    }

    /// Destroys an EGL sync object
    fn destroy_egl_sync(&self, sync: *mut std::ffi::c_void) {
        unsafe {
            // Get EGL bindings from glutin display
            let egl = self.gles_context.display().egl();
            let egl_display = egl.GetCurrentDisplay();

            // Destroy the sync object
            egl.DestroySyncKHR(egl_display, sync);
        }
    }

    /// Checks if a mode set is needed (internal).
    ///
    /// Returns `true` if `requested_mode` differs from `current_mode`,
    /// indicating that a mode set should be included in the next atomic commit.
    pub(crate) fn needs_mode_set(&self) -> bool {
        self.requested_mode != self.current_mode
    }

    /// Marks the mode as successfully set (internal).
    ///
    /// Called after a successful atomic commit with mode setting.
    /// Updates `current_mode` to match `requested_mode`.
    pub(crate) fn mark_mode_set(&mut self) {
        self.current_mode = self.requested_mode;
    }

    /// Clears the current mode state (internal).
    ///
    /// Called when TTY focus is lost or monitor needs re-initialization.
    /// This will trigger a mode set on the next frame.
    ///
    /// TODO: This will be used for TTY focus handling (SIGUSR1/SIGUSR2 signals)
    #[allow(dead_code)]
    pub(crate) fn clear_mode_state(&mut self) {
        self.current_mode = None;
        self.first_frame = true; // Will need full mode set on next frame
    }

    /// Gets a reference to the OpenGL ES bindings.
    ///
    /// This provides direct access to all OpenGL ES functions for this monitor.
    /// The GL context is automatically initialized when the monitor is created.
    ///
    /// # Example
    ///
    /// ```ignore
    /// monitor.make_current()?;
    /// let gl = monitor.gl();
    /// unsafe {
    ///     gl.ClearColor(0.0, 0.0, 0.0, 1.0);
    ///     gl.Clear(gl::COLOR_BUFFER_BIT);
    /// }
    /// ```
    pub fn gl(&self) -> &crate::gl::Gles2 {
        self.gles_context.gl()
    }

    /// Gets a function pointer for loading OpenGL functions.
    ///
    /// This is provided for compatibility with external libraries that need
    /// to load their own GL function pointers. Most users should use `gl()` instead.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // For use with external libraries
    /// external_lib::load_with(|symbol| monitor.get_proc_address(symbol));
    /// ```
    pub fn get_proc_address(&self, symbol: &str) -> *const std::ffi::c_void {
        self.gles_context.get_proc_address(symbol)
    }

    /// Returns the DRM connector handle for this monitor.
    pub fn connector_id(&self) -> connector::Handle {
        self.connector_id
    }

    /// Returns the CRTC information for this monitor.
    pub fn crtc(&self) -> &crtc::Info {
        &self.current_crtc
    }

    /// Returns the optimal display mode for this monitor.
    ///
    /// This is the preferred mode reported by the monitor (typically the highest
    /// resolution and refresh rate supported, e.g., 4K@120Hz).
    pub fn default_mode(&self) -> &control::Mode {
        &self.default_mode
    }

    /// Returns the mode currently set in hardware.
    ///
    /// Returns `None` if no mode has been set yet (new monitor or after TTY focus loss).
    /// Returns `Some(&Mode)` if a mode has been successfully committed to DRM.
    ///
    /// This may differ from `requested_mode()` briefly during mode transitions.
    pub fn current_mode(&self) -> Option<&control::Mode> {
        self.current_mode.as_ref()
    }

    /// Returns the mode that should be used for rendering.
    ///
    /// Returns `None` if the optimal default mode should be used.
    /// Returns `Some(&Mode)` if a custom mode has been requested
    /// (e.g., downscaled to 1080p or lower refresh rate).
    pub fn requested_mode(&self) -> Option<&control::Mode> {
        self.requested_mode.as_ref()
    }

    /// Sets the requested display mode for this monitor.
    ///
    /// This allows using a different mode than the optimal default
    /// (e.g., lower resolution or refresh rate for compatibility).
    ///
    /// The mode change will take effect on the next `swap_buffers()` call
    /// if it differs from the current hardware state.
    ///
    /// # Arguments
    ///
    /// * `mode` - The new display mode, or `None` to revert to the optimal default
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Use 1080p@60Hz instead of the default 4K@120Hz
    /// let mode_1080p = find_mode_by_resolution(monitor, 1920, 1080);
    /// monitor.set_mode(Some(mode_1080p));
    ///
    /// // Revert to optimal default
    /// monitor.set_mode(None);
    /// ```
    pub fn set_mode(&mut self, mode: Option<control::Mode>) {
        self.requested_mode = mode;
    }

    /// Returns the effective display mode for rendering.
    ///
    /// This is a convenience method that returns the requested mode if set,
    /// otherwise returns the optimal default mode.
    ///
    /// Use this to determine what resolution/refresh rate to render at.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let active = monitor.active_mode();
    /// println!("Rendering at {}x{} @ {}Hz",
    ///     active.size().0, active.size().1, active.vrefresh());
    /// ```
    pub fn active_mode(&self) -> &control::Mode {
        self.requested_mode.as_ref().unwrap_or(&self.default_mode)
    }

    /// Returns the handle to the primary plane used for scanout.
    pub fn primary_plane(&self) -> plane::Handle {
        self.primary_plane_id
    }

    /// Returns the handle to the cursor plane, if available.
    ///
    /// Some GPUs (especially virtual ones like virtio-gpu) don't expose a separate cursor plane.
    pub fn cursor_plane(&self) -> Option<plane::Handle> {
        self.cursor_plane_id
    }

    /// Returns the current resolution as (width, height).
    ///
    /// Uses the requested mode if one has been set, otherwise returns
    /// the optimal default mode's resolution.
    pub fn size(&self) -> (u16, u16) {
        self.active_mode().size()
    }
}

impl<T> Drop for Monitor<T> {
    fn drop(&mut self) {
        // Clean up fence resources to prevent FD leaks
        if let Some(fence_fd) = self.previous_fence_fd.take() {
            unsafe {
                libc::close(fence_fd);
            }
        }
        if let Some(sync) = self.previous_sync.take() {
            self.destroy_egl_sync(sync);
        }
    }
}
