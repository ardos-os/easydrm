//! # EasyDRM - Minimal DRM/KMS Framework
//!
//! EasyDRM provides a simple, GLFW-like API for direct rendering on Linux without
//! a compositor (no X11, no Wayland). It's designed for fullscreen applications,
//! kiosks, embedded systems, or custom compositors.
//!
//! ## Features
//!
//! - **Multi-monitor support** with independent OpenGL ES contexts per monitor
//! - **Hot-plug detection** - monitors can be added/removed at runtime
//! - **Zero-monitor graceful handling** - doesn't crash without monitors
//! - **Per-monitor user context** - attach custom data (Skia, Cairo, etc.)
//! - **Refresh-rate grouping metadata** for diagnostics or custom scheduling strategies
//! - **Atomic commits** with proper fence synchronization
//! - **3-state mode management** for efficient mode setting
//!
//! ## Basic Usage
//!
//! ```no_run
//! use easydrm::EasyDRM;
//!
//! // Initialize without custom context
//! let mut easydrm = EasyDRM::init_empty().unwrap();
//!
//! loop {
//!     // Wait for events (page flip, hotplug, etc.)
//!     easydrm.poll_events().unwrap();
//!
//!     // Render to each monitor that's ready
//!     for monitor in easydrm.monitors() {
//!         if monitor.can_render() {
//!             monitor.make_current().unwrap();
//!             let gl = monitor.gl();
//!             unsafe {
//!                 gl.Clear(gl::COLOR_BUFFER_BIT);
//!             }
//!         }
//!     }
//!
//!     // Commit all drawn monitors
//!     easydrm.swap_buffers().unwrap();
//! }
//! ```
//!
//! ## With Custom Context
//!
//! ```no_run
//! use easydrm::EasyDRM;
//!
//! struct MyContext {
//!     frame_count: u32,
//! }
//!
//! let mut easydrm = EasyDRM::init(|req| {
//!     println!(
//!         "Monitor init: {}x{}, get symbol ptr example: {:?}",
//!         req.width,
//!         req.height,
//!         (req.get_proc_address)("glCreateShader")
//!     );
//!     MyContext { frame_count: 0 }
//! })
//! .unwrap();
//!
//! for monitor in easydrm.monitors() {
//!     let ctx = monitor.context_mut();
//!     ctx.frame_count += 1;
//! }
//! ```

use std::collections::{HashMap, HashSet};
use std::future::pending;
use std::os::fd::{AsFd, BorrowedFd, RawFd};
use std::os::unix::io::AsRawFd;

use drm::Device;
use drm::control::atomic::AtomicModeReq;
use drm::control::{
    AtomicCommitFlags, Device as ControlDevice, Event, PlaneType, connector, crtc, plane,
};
use gbm::Device as GbmDevice;
use libc::{F_GETFL, F_SETFL, O_NONBLOCK, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use thiserror::Error;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

use crate::card::Card;
use crate::monitor::{MonitorResourceAllocation, MonitorSetupError};

mod card;
mod gles_context;
mod hotplug;
mod monitor;

// Public API exports
pub use monitor::Monitor;

/// OpenGL ES bindings generated at build time
#[allow(clippy::all, warnings)]
pub mod gl {
    include!(concat!(env!("OUT_DIR"), "/gl_bindings.rs"));
}

/// Data provided to the user callback when creating a per-monitor context.
pub struct MonitorContextCreationRequest<'a> {
    /// OpenGL ES bindings for this monitor.
    pub gl: &'a crate::gl::Gles2,
    /// Width of the monitor surface in pixels.
    pub width: usize,
    /// Height of the monitor surface in pixels.
    pub height: usize,
    /// Helper to load extra GL symbols (useful for third-party loaders).
    pub get_proc_address: &'a dyn Fn(&str) -> *const std::ffi::c_void,
}

#[derive(Debug, Error)]
pub enum EasyDRMError {
    #[error("IO Error: {0}")]
    IOError(#[from] std::io::Error),
    #[error("Monitor setup error: {0}")]
    MonitorSetup(#[from] MonitorSetupError),
}

pub struct EasyDRM<T> {
    card: Card,
    gbm_device: GbmDevice<std::fs::File>,
    monitors: HashMap<connector::Handle, Monitor<T>>,
    refresh_rate_groups: HashMap<u32, Vec<connector::Handle>>, // refresh_rate -> connector handles
    fastest_group_refresh: Option<u32>,
    fastest_group_pending: HashSet<connector::Handle>,
    should_update_flag: bool,
    context_constructor: Box<dyn for<'a> Fn(&MonitorContextCreationRequest<'a>) -> T + 'static>,
    uevent_socket: Option<hotplug::UEventSocket>,
}

impl<T> EasyDRM<T> {
    /// Initialize EasyDRM with a custom context constructor for each monitor
    ///
    /// The context_constructor receives a [`MonitorContextCreationRequest`] with access to GL
    /// bindings, framebuffer size, and a `get_proc_address` helper so you can load extra
    /// OpenGL functions during initialization. Use this to set up per-monitor resources
    /// (Skia surfaces, Cairo contexts, etc.)
    ///
    /// # Example
    ///
    /// ```ignore
    /// struct MyContext {
    ///     skia_surface: skia::Surface,
    /// }
    ///
    /// let easydrm = EasyDRM::init(|req| {
    ///     MyContext {
    ///         skia_surface: create_skia_surface(req.gl),
    ///     }
    /// })?;
    /// ```
    ///
    /// Note: EasyDRM will successfully initialize even with zero monitors connected as long as you have a GPU.
    /// Monitors can be hot-plugged later and will be automatically discovered via `poll_events()`.
    pub fn init<F>(context_constructor: F) -> Result<Self, EasyDRMError>
    where
        F: for<'a> Fn(&MonitorContextCreationRequest<'a>) -> T + 'static,
    {
        // Open DRM card
        let card = Card::open_default_card();

        // Enable required capabilities
        card.set_client_capability(drm::ClientCapability::UniversalPlanes, true)
            .expect("Unable to request UniversalPlanes capability");
        card.set_client_capability(drm::ClientCapability::Atomic, true)
            .expect("Unable to request Atomic capability");

        // Create GBM device (needs ownership, so we clone the file descriptor)
        let fd = card.as_raw_fd();
        let gbm_device = unsafe {
            use std::os::unix::io::FromRawFd;
            GbmDevice::new(std::fs::File::from_raw_fd(libc::dup(fd)))
                .expect("Failed to create GBM device")
        };

        let mut easydrm = EasyDRM {
            card,
            gbm_device,
            monitors: HashMap::new(),
            refresh_rate_groups: HashMap::new(),
            fastest_group_refresh: None,
            fastest_group_pending: HashSet::new(),
            should_update_flag: false,
            context_constructor: Box::new(context_constructor),
            uevent_socket: hotplug::UEventSocket::open().ok(),
        };
        if easydrm.uevent_socket.is_none() {
            eprintln!(
                "[WARNING] Failed to open uevent socket, monitors plugged in afterwards won't be able to be detected"
            );
        }
        // Discover and initialize all connected monitors
        easydrm.discover_monitors()?;

        // It's OK to have zero monitors - they might be connected later via hotplug

        Ok(easydrm)
    }

    /// Discover all connected monitors and initialize them
    fn discover_monitors(&mut self) -> Result<(), EasyDRMError> {
        let res = self.card.resource_handles()?;

        // Get all connector handles
        let connector_handles: Vec<connector::Handle> = res.connectors().to_vec();
        let (mut used_crtcs, mut used_primary_planes, mut used_cursor_planes) =
            self.current_resource_usage();

        // Setup monitors
        for connector_id in connector_handles {
            if self.monitors.contains_key(&connector_id) {
                continue;
            }

            let allocation = match self.allocate_monitor_resources(
                connector_id,
                &used_crtcs,
                &used_primary_planes,
                &used_cursor_planes,
            ) {
                Ok(allocation) => allocation,
                Err(e) => {
                    eprintln!(
                        "Warning: Failed to allocate resources for monitor {:?}: {}",
                        connector_id, e
                    );
                    continue;
                }
            };

            match Monitor::setup(
                &self.card,
                &self.gbm_device,
                connector_id,
                allocation,
                |request| (self.context_constructor)(request),
            ) {
                Ok(monitor) => {
                    used_crtcs.insert(monitor.crtc().handle());
                    used_primary_planes.insert(monitor.primary_plane());
                    if let Some(cursor) = monitor.cursor_plane() {
                        used_cursor_planes.insert(cursor);
                    }
                    self.monitors.insert(connector_id, monitor);
                }
                Err(e) => {
                    eprintln!("Warning: Failed to setup monitor {:?}: {}", connector_id, e);
                }
            }
        }

        self.update_refresh_rate_groups();

        Ok(())
    }

    /// Update refresh rate groups based on current monitors
    fn update_refresh_rate_groups(&mut self) {
        self.refresh_rate_groups.clear();

        for (&connector_id, monitor) in self.monitors.iter() {
            let refresh_rate = monitor.active_mode().vrefresh();
            self.refresh_rate_groups
                .entry(refresh_rate)
                .or_default()
                .push(connector_id);
        }

        self.fastest_group_refresh = self.refresh_rate_groups.keys().max().copied();
        self.reset_fastest_group_pending();
    }

    fn reset_fastest_group_pending(&mut self) {
        self.fastest_group_pending.clear();
        if let Some(refresh) = self.fastest_group_refresh {
            if let Some(connectors) = self.refresh_rate_groups.get(&refresh) {
                self.fastest_group_pending
                    .extend(connectors.iter().copied());
            }
        }
        // If there is no fastest group (i.e., no monitors), keep the flag false.
        self.should_update_flag =
            self.fastest_group_pending.is_empty() && self.fastest_group_refresh.is_some();
    }

    fn mark_fast_group_commit(&mut self, connector_id: connector::Handle) {
        if self.fastest_group_pending.remove(&connector_id) && self.fastest_group_pending.is_empty()
        {
            self.should_update_flag = true;
        }
    }

    /// Handle hotplug events - add/remove monitors as needed
    fn handle_hotplug(&mut self) -> Result<(), EasyDRMError> {
        let res = self.card.resource_handles()?;

        // Get current connected monitors from DRM
        let drm_connected = res
            .connectors()
            .into_iter()
            .copied()
            .collect::<HashSet<_>>();

        // Get current monitor connector IDs
        let current_monitors: HashSet<connector::Handle> = self.monitors.keys().copied().collect();

        // Find monitors that were disconnected
        let disconnected: Vec<connector::Handle> = current_monitors
            .difference(&drm_connected)
            .copied()
            .collect();

        // Find monitors that were newly connected
        let newly_connected: Vec<connector::Handle> = drm_connected
            .difference(&current_monitors)
            .copied()
            .collect();

        // Check if we need to update groups before consuming the vectors
        let needs_update = !disconnected.is_empty() || !newly_connected.is_empty();

        // Remove disconnected monitors
        for connector_id in disconnected {
            self.monitors.retain(|&c, _| c != connector_id);
        }

        let (mut used_crtcs, mut used_primary_planes, mut used_cursor_planes) =
            self.current_resource_usage();

        // Add newly connected monitors
        for connector_id in newly_connected {
            let allocation = match self.allocate_monitor_resources(
                connector_id,
                &used_crtcs,
                &used_primary_planes,
                &used_cursor_planes,
            ) {
                Ok(allocation) => allocation,
                Err(e) => {
                    eprintln!(
                        "Warning: Failed to allocate resources for monitor {:?}: {}",
                        connector_id, e
                    );
                    continue;
                }
            };

            match Monitor::setup(
                &self.card,
                &self.gbm_device,
                connector_id,
                allocation,
                |request| (self.context_constructor)(request),
            ) {
                Ok(monitor) => {
                    used_crtcs.insert(monitor.crtc().handle());
                    used_primary_planes.insert(monitor.primary_plane());
                    if let Some(cursor) = monitor.cursor_plane() {
                        used_cursor_planes.insert(cursor);
                    }
                    self.monitors.insert(connector_id, monitor);
                }
                Err(e) => {
                    eprintln!("Warning: Failed to setup monitor {:?}: {}", connector_id, e);
                }
            }
        }

        // Update refresh rate groups if monitors changed
        if needs_update {
            self.update_refresh_rate_groups();
        }

        Ok(())
    }

    fn current_resource_usage(
        &self,
    ) -> (
        HashSet<crtc::Handle>,
        HashSet<plane::Handle>,
        HashSet<plane::Handle>,
    ) {
        let mut used_crtcs = HashSet::new();
        let mut used_primary_planes = HashSet::new();
        let mut used_cursor_planes = HashSet::new();

        for monitor in self.monitors.values() {
            used_crtcs.insert(monitor.crtc().handle());
            used_primary_planes.insert(monitor.primary_plane());
            if let Some(cursor) = monitor.cursor_plane() {
                used_cursor_planes.insert(cursor);
            }
        }

        (used_crtcs, used_primary_planes, used_cursor_planes)
    }

    fn allocate_monitor_resources(
        &self,
        connector_id: connector::Handle,
        used_crtcs: &HashSet<crtc::Handle>,
        used_primary_planes: &HashSet<plane::Handle>,
        used_cursor_planes: &HashSet<plane::Handle>,
    ) -> Result<MonitorResourceAllocation, MonitorSetupError> {
        let connector = self.card.get_connector(connector_id, true)?;
        if connector.state() != connector::State::Connected {
            return Err(MonitorSetupError::NotConnected);
        }

        let res = self.card.resource_handles()?;
        let crtc_candidates = self.crtc_candidates_for_connector(&connector, &res, used_crtcs)?;
        let planes = self.card.plane_handles()?;
        let plane_handles: Vec<plane::Handle> = planes.iter().copied().collect();

        let mut crtc_info = None;
        for handle in crtc_candidates {
            if let Ok(info) = self.card.get_crtc(handle) {
                crtc_info = Some(info);
                break;
            }
        }
        let crtc_info = crtc_info.ok_or(MonitorSetupError::NoCRTCFound)?;

        let primary_plane = self
            .find_plane_for_crtc(
                &plane_handles,
                &res,
                crtc_info.handle(),
                PlaneType::Primary,
                used_primary_planes,
            )?
            .ok_or(MonitorSetupError::NoPrimaryPlaneFound)?;

        let cursor_plane = self.find_plane_for_crtc(
            &plane_handles,
            &res,
            crtc_info.handle(),
            PlaneType::Cursor,
            used_cursor_planes,
        )?;

        Ok(MonitorResourceAllocation {
            crtc_info,
            primary_plane,
            cursor_plane,
        })
    }

    fn crtc_candidates_for_connector(
        &self,
        connector: &connector::Info,
        res: &drm::control::ResourceHandles,
        used_crtcs: &HashSet<crtc::Handle>,
    ) -> Result<Vec<crtc::Handle>, MonitorSetupError> {
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();

        for encoder_handle in connector.encoders() {
            let Ok(encoder) = self.card.get_encoder(*encoder_handle) else {
                continue;
            };

            for crtc_handle in res.filter_crtcs(encoder.possible_crtcs()) {
                if used_crtcs.contains(&crtc_handle) {
                    continue;
                }
                if seen.insert(crtc_handle) {
                    candidates.push(crtc_handle);
                }
            }
        }

        Ok(candidates)
    }

    fn find_plane_for_crtc(
        &self,
        plane_handles: &[plane::Handle],
        res: &drm::control::ResourceHandles,
        crtc_handle: crtc::Handle,
        plane_type: PlaneType,
        used_planes: &HashSet<plane::Handle>,
    ) -> Result<Option<plane::Handle>, MonitorSetupError> {
        for plane_handle in plane_handles {
            if used_planes.contains(plane_handle) {
                continue;
            }

            let Ok(plane_info) = self.card.get_plane(*plane_handle) else {
                continue;
            };
            let compatible_crtcs = res.filter_crtcs(plane_info.possible_crtcs());
            if !compatible_crtcs.contains(&crtc_handle) {
                continue;
            }

            if self.plane_is_type(*plane_handle, plane_type)? {
                return Ok(Some(*plane_handle));
            }
        }

        Ok(None)
    }

    fn plane_is_type(
        &self,
        plane_handle: plane::Handle,
        plane_type: PlaneType,
    ) -> Result<bool, MonitorSetupError> {
        let properties = self.card.get_properties(plane_handle)?;
        for (&id, &value) in properties.iter() {
            let Ok(info) = self.card.get_property(id) else {
                continue;
            };

            if info
                .name()
                .to_str()
                .map(|name| name == "type")
                .unwrap_or(false)
            {
                return Ok(value == (plane_type as u32).into());
            }
        }

        Ok(false)
    }
    /// Poll for events (page flip, hotplug, etc.)
    /// This blocks until an event is received
    pub fn poll_events(&mut self) -> Result<(), EasyDRMError> {
        self.poll_events_ex([])
    }
    /// Extended version of [[poll_events]] that allows waiting for additional fds
    pub fn poll_events_ex(
        &mut self,
        extra_fds: impl IntoIterator<Item = RawFd>,
    ) -> Result<(), EasyDRMError> {
        let drm_fd = self.card.as_fd();
        let uevents_socket = self.uevent_socket.as_ref();

        // preparar descritores para poll
        let mut fds = vec![PollFd::new(drm_fd, PollFlags::POLLIN)];
        fds.extend(
            extra_fds
                .into_iter()
                .map(|f| PollFd::new(unsafe { BorrowedFd::borrow_raw(f) }, PollFlags::POLLIN)),
        );
        if let Some(uevents_socket) = uevents_socket {
            fds.push(PollFd::new(uevents_socket.fd.as_fd(), PollFlags::POLLIN));
        }

        // bloquear até haver evento
        poll(&mut fds, PollTimeout::NONE).ok();

        let drm_ready = fds[0]
            .revents()
            .unwrap_or(PollFlags::empty())
            .contains(PollFlags::POLLIN);

        if let Some(uevents_socket) = uevents_socket {
            let hotplug_ready = fds
                .iter()
                .find(|p| p.as_fd().as_raw_fd() == uevents_socket.fd.as_raw_fd())
                .and_then(|p| p.revents())
                .unwrap_or(PollFlags::empty())
                .contains(PollFlags::POLLIN);

            if hotplug_ready && uevents_socket.drain_hotplug_events().unwrap_or(false) {
                println!("[INFO] Hotplug detected, refreshing monitors.");
                self.handle_hotplug()?;
            }
        }

        if drm_ready {
            Self::handle_drm_events(&mut self.card, &mut self.monitors)?;
        }
        Ok(())
    }
    /// Non-blocking version of [[poll_events]]
    pub async fn poll_events_async(
        &mut self
    ) -> Result<(), EasyDRMError> {
        let drm_sync_fd = self.card.as_raw_fd();
        let drm_async_fd = AsyncFd::with_interest(drm_sync_fd, Interest::READABLE)?;
        let uevents_raw_fd = self.uevent_socket.as_ref().map(|s| s.fd.as_raw_fd());
        unsafe {

            fcntl(drm_sync_fd, F_SETFL, fcntl(drm_sync_fd, F_GETFL)|O_NONBLOCK);
            if let Some(fd) = uevents_raw_fd {
                fcntl(fd, F_SETFL, fcntl(fd, F_GETFL)|O_NONBLOCK);
            }
        }
        let uevents_async_fd =
            self.uevent_socket.as_ref()
            .map(|s| AsyncFd::with_interest(s.fd.as_raw_fd(), Interest::READABLE))
            .transpose()?;
        tokio::select! {
            guard = drm_async_fd.readable() => {
                let mut guard = guard?;
                if let Ok(r) = guard.try_io(|_fd| {
                    Self::handle_drm_events(&mut self.card, &mut self.monitors)
                }) { r? }
            }

            _ = async {
                if let Some(ref uevents_async_fd) = uevents_async_fd {
                    uevents_async_fd.readable().await
                } else {
                    pending().await
                }
            } => {
                if let Some(ue) = self.uevent_socket.as_ref()
                && ue.drain_hotplug_events().unwrap_or(false) {
                    self.handle_hotplug()?;
                }
            }
        }
        Ok(())
    }

    /// Get an iterator over all monitors
    pub fn monitors(&self) -> impl Iterator<Item = &Monitor<T>> {
        self.monitors.values()
    }

    /// Get an iterator over all monitors
    pub fn monitors_mut(&mut self) -> impl Iterator<Item = &mut Monitor<T>> {
        self.monitors.values_mut()
    }

    /// Get a specific monitor by connector handle
    pub fn get_monitor_mut(&mut self, connector_id: connector::Handle) -> Option<&mut Monitor<T>> {
        self.monitors.get_mut(&connector_id)
    }

    /// Get a specific monitor by connector handle
    pub fn get_monitor(&self, connector_id: connector::Handle) -> Option<&Monitor<T>> {
        self.monitors.get(&connector_id)
    }

    /// Swap buffers for all monitors that were drawn to.
    ///
    /// Each monitor that set `was_drawn = true` during this frame gets its own
    /// `Monitor::swap_buffers()` call, which issues the atomic commit and
    /// fence hand-off for that monitor.
    pub fn swap_buffers(&mut self) -> Result<(), EasyDRMError> {
        let mut atomic_req = AtomicModeReq::new();
        // Determine commit flags
        let mut flags = AtomicCommitFlags::PAGE_FLIP_EVENT;
        // Rebuild the set with swapped monitors
        let mut committed = Vec::new();
        for (&connector_id, monitor) in self.monitors.iter_mut() {
            if monitor.was_drawn() {
            		if monitor.needs_mode_set() {
              		flags |= AtomicCommitFlags::ALLOW_MODESET;
              	}
                monitor.swap_buffers(&self.card, &mut atomic_req)?;
                monitor.reset_drawn_flag();
                committed.push(connector_id);
            }
        }
        let should_commit = !committed.is_empty();
        for connector_id in committed {
            self.mark_fast_group_commit(connector_id);
        }

        // Submit atomic commit (queues the page flip, doesn't wait)
        if should_commit {
            self.card
                .atomic_commit(flags, atomic_req)
                .map_err(|e| MonitorSetupError::DrmError(format!("Failed to commit: {}", e)))?;
        }
        Ok(())
    }

    /// Get the number of connected monitors
    pub fn monitor_count(&self) -> usize {
        self.monitors.len()
    }

    /// Check if there are any monitors connected
    pub fn has_monitors(&self) -> bool {
        !self.monitors.is_empty()
    }

    /// Check if any monitor can render
    pub fn any_can_render(&self) -> bool {
        self.monitors.values().any(|m| m.can_render())
    }

    /// Get monitors grouped by refresh rate.
    ///
    /// This is currently informational for callers that want to drive custom
    /// scheduling policies; EasyDRM itself does not yet alter timing based on
    /// these groups.
    pub fn refresh_rate_groups(&self) -> &HashMap<u32, Vec<connector::Handle>> {
        &self.refresh_rate_groups
    }

    /// Returns true once after every cycle where all monitors in the fastest refresh
    /// rate group have swapped their buffers.
    ///
    /// Call this from your main loop to synchronize global logic (simulation, input
    /// processing, etc.) to the fastest monitor's cadence.
    pub fn should_update(&mut self) -> bool {
        if self.should_update_flag {
            self.should_update_flag = false;
            self.reset_fastest_group_pending();
            true
        } else {
            false
        }
    }

    fn handle_drm_events(card: &mut Card, monitors: &mut HashMap<connector::Handle, Monitor<T>>) -> std::io::Result<()> {
        // Wait for events from DRM
        for event in card.receive_events()? {
            match event {
                Event::PageFlip(page_flip_event) => {
                    // Find the monitor that completed the page flip
                    // Set can_render = true for that monitor
                    let crtc_handle = page_flip_event.crtc;

                    for monitor in monitors.values_mut() {
                        if monitor.crtc().handle() == crtc_handle {
                            monitor.set_can_render(true);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

// Special implementation for unit type - no user context
impl EasyDRM<()> {
    /// Initialize EasyDRM without any custom context
    ///
    /// This is a convenience method for when you don't need per-monitor data.
    /// Equivalent to `EasyDRM::init(|_| ())`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let easydrm = EasyDRM::init_empty()?;
    ///
    /// for monitor in easydrm.monitors() {
    ///     // No custom context, just use GL directly
    ///     let gl = monitor.gl();
    ///     // ...
    /// }
    /// ```
    pub fn init_empty() -> Result<Self, EasyDRMError> {
        Self::init(|_| ())
    }
}
