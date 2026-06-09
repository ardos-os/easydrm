use std::{
    collections::HashMap,
    hash::Hash,
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    sync::{Arc, Mutex},
};

use drm::control::{self, FbCmd2Flags, atomic::AtomicModeReq, connector, crtc, plane, property};
use gbm::{BufferObject, BufferObjectFlags, Device as GbmDevice, Format, Modifier};
use glutin::display::AsRawDisplay;
use thiserror::Error;

use crate::MonitorContextCreationRequest;
use crate::gles_context::{GlesContext, GlesContextError};

/// DRM resources dedicated to a monitor instance.
pub(crate) struct MonitorResourceAllocation {
    pub crtc_info: crtc::Info,
    pub primary_plane: plane::Handle,
    pub cursor_plane: Option<plane::Handle>,
}

const EGL_NONE: i32 = 0x3038;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
const EGL_SYNC_FENCE_KHR: u32 = 0x3144;

struct SwapchainSlot {
    bo: BufferObject<()>,
    fbo: u32,
    texture: u32,
    egl_image: *mut std::ffi::c_void,
    drm_fb: Option<control::framebuffer::Handle>,
}

/// Represents a connected display monitor with a shared surfaceless GL context
/// and a monitor-local explicit GBM swapchain.
pub struct Monitor<T> {
    connector_id: connector::Handle,
    current_crtc: crtc::Info,
    default_mode: control::Mode,
    requested_mode: Option<control::Mode>,
    current_mode: Option<control::Mode>,
    primary_plane_id: plane::Handle,
    cursor_plane_id: Option<plane::Handle>,
    gles_context: Arc<Mutex<GlesContext>>,
    gbm_device: GbmDevice<std::fs::File>,
    gl: crate::gl::Gles2,
    swapchain: [SwapchainSlot; 2],
    current_slot: usize,
    can_render: bool,
    was_drawn: bool,
    connector_properties: HashMap<String, property::Info>,
    crtc_properties: HashMap<String, property::Info>,
    plane_properties: HashMap<String, property::Info>,
    first_frame: bool,
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

fn create_slot(
    ctx: &GlesContext,
    gl: &crate::gl::Gles2,
    gbm_device: &GbmDevice<std::fs::File>,
    mode: &control::Mode,
) -> Result<SwapchainSlot, MonitorSetupError> {
    let (width, height) = mode.size();
    let bo = gbm_device
        .create_buffer_object::<()>(
            width.into(),
            height.into(),
            Format::Xrgb8888,
            BufferObjectFlags::SCANOUT | BufferObjectFlags::RENDERING,
        )
        .map_err(MonitorSetupError::IOError)?;

    let egl = ctx.display().egl();
    let fd = bo
        .fd()
        .map_err(|_| MonitorSetupError::DrmError("Failed to export BO dmabuf fd".into()))?;

    let mut attrs = vec![
        EGL_WIDTH,
        bo.width() as i32,
        EGL_HEIGHT,
        bo.height() as i32,
        EGL_LINUX_DRM_FOURCC_EXT,
        bo.format() as u32 as i32,
        EGL_DMA_BUF_PLANE0_FD_EXT,
        fd.as_raw_fd(),
        EGL_DMA_BUF_PLANE0_OFFSET_EXT,
        bo.offset(0) as i32,
        EGL_DMA_BUF_PLANE0_PITCH_EXT,
        bo.stride() as i32,
    ];
    let bo_modifier = bo.modifier();
    if bo_modifier != Modifier::Invalid {
        let modifier: u64 = bo_modifier.into();
        attrs.extend_from_slice(&[
            EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            (modifier & 0xFFFF_FFFF) as i32,
            EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            ((modifier >> 32) & 0xFFFF_FFFF) as i32,
        ]);
    }
    attrs.push(EGL_NONE);

    let egl_image = unsafe {
        egl.CreateImageKHR(
            egl.GetCurrentDisplay(),
            std::ptr::null_mut(),
            EGL_LINUX_DMA_BUF_EXT,
            std::ptr::null(),
            attrs.as_ptr(),
        )
    };

    if egl_image.is_null() {
        return Err(MonitorSetupError::DrmError(
            "eglCreateImageKHR failed for monitor swapchain slot".into(),
        ));
    }

    let mut texture: u32 = 0;
    let mut fbo: u32 = 0;
    unsafe {
        gl.GenTextures(1, &mut texture);
        gl.BindTexture(crate::gl::TEXTURE_2D, texture);
        gl.TexParameteri(
            crate::gl::TEXTURE_2D,
            crate::gl::TEXTURE_MIN_FILTER,
            crate::gl::LINEAR as i32,
        );
        gl.TexParameteri(
            crate::gl::TEXTURE_2D,
            crate::gl::TEXTURE_MAG_FILTER,
            crate::gl::LINEAR as i32,
        );
        gl.TexParameteri(
            crate::gl::TEXTURE_2D,
            crate::gl::TEXTURE_WRAP_S,
            crate::gl::CLAMP_TO_EDGE as i32,
        );
        gl.TexParameteri(
            crate::gl::TEXTURE_2D,
            crate::gl::TEXTURE_WRAP_T,
            crate::gl::CLAMP_TO_EDGE as i32,
        );
        gl.EGLImageTargetTexture2DOES(crate::gl::TEXTURE_2D, egl_image.cast());
        gl.BindTexture(crate::gl::TEXTURE_2D, 0);

        gl.GenFramebuffers(1, &mut fbo);
        gl.BindFramebuffer(crate::gl::FRAMEBUFFER, fbo);
        gl.FramebufferTexture2D(
            crate::gl::FRAMEBUFFER,
            crate::gl::COLOR_ATTACHMENT0,
            crate::gl::TEXTURE_2D,
            texture,
            0,
        );

        let status = gl.CheckFramebufferStatus(crate::gl::FRAMEBUFFER);
        gl.BindFramebuffer(crate::gl::FRAMEBUFFER, 0);
        if status != crate::gl::FRAMEBUFFER_COMPLETE {
            return Err(MonitorSetupError::DrmError(format!(
                "Failed to create framebuffer, status={status:#x}"
            )));
        }
    }

    Ok(SwapchainSlot {
        bo,
        fbo,
        texture,
        egl_image: egl_image as *mut std::ffi::c_void,
        drm_fb: None,
    })
}

fn destroy_slot(ctx: &GlesContext, gl: &crate::gl::Gles2, slot: &SwapchainSlot) {
    let egl = ctx.display().egl();
    unsafe {
        if slot.fbo != 0 {
            gl.DeleteFramebuffers(1, &slot.fbo);
        }
        if slot.texture != 0 {
            gl.DeleteTextures(1, &slot.texture);
        }
        if !slot.egl_image.is_null() {
            egl.DestroyImageKHR(egl.GetCurrentDisplay(), slot.egl_image);
        }
    }
}

impl<T> Monitor<T> {
    pub(crate) fn setup<F>(
        card: &impl control::Device,
        gbm_device: &GbmDevice<std::fs::File>,
        shared_gles_context: Arc<Mutex<GlesContext>>,
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

        let default_mode = connector
            .modes()
            .first()
            .cloned()
            .ok_or(MonitorSetupError::NoModesFound)?;

        let gbm_device_clone = {
            use std::os::fd::FromRawFd;
            let fd = gbm_device.as_fd().as_raw_fd();
            let dupfd = unsafe { libc::dup(fd) };
            if dupfd < 0 {
                return Err(MonitorSetupError::DrmError("Failed to dup gbm fd".into()));
            }
            let file = unsafe { std::fs::File::from_raw_fd(dupfd) };
            GbmDevice::new(file).map_err(MonitorSetupError::IOError)?
        };

        let gl = {
            let ctx = shared_gles_context
                .lock()
                .map_err(|_| MonitorSetupError::DrmError("Shared GLES context poisoned".into()))?;
            ctx.gl().clone()
        };

        let (slot0, slot1) = {
            let ctx = shared_gles_context
                .lock()
                .map_err(|_| MonitorSetupError::DrmError("Shared GLES context poisoned".into()))?;
            ctx.make_current_surfaceless()?;
            (
                create_slot(&ctx, &gl, &gbm_device_clone, &default_mode)?,
                create_slot(&ctx, &gl, &gbm_device_clone, &default_mode)?,
            )
        };

        unsafe {
            gl.BindFramebuffer(crate::gl::FRAMEBUFFER, slot0.fbo);
            gl.Viewport(0, 0, slot0.bo.width() as i32, slot0.bo.height() as i32);
        }

        let get_proc_address = |symbol: &str| {
            shared_gles_context
                .lock()
                .map(|ctx| ctx.get_proc_address(symbol))
                .unwrap_or(std::ptr::null())
        };
        let request = MonitorContextCreationRequest {
            gl: &gl,
            width: default_mode.size().0 as usize,
            height: default_mode.size().1 as usize,
            get_proc_address: &get_proc_address,
        };
        let user_context = context_constructor(&request);

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
            requested_mode: None,
            current_mode: None,
            primary_plane_id: primary_plane,
            cursor_plane_id: cursor_plane,
            gles_context: shared_gles_context,
            gbm_device: gbm_device_clone,
            gl,
            swapchain: [slot0, slot1],
            current_slot: 0,
            can_render: true,
            was_drawn: false,
            connector_properties,
            crtc_properties,
            plane_properties,
            first_frame: true,
            user_context,
        })
    }

    fn ensure_swapchain_for_mode(
        &mut self,
        card: &impl control::Device,
        mode: &control::Mode,
    ) -> Result<(), MonitorSetupError> {
        let (w, h) = mode.size();
        if self.swapchain[0].bo.width() == w as u32 && self.swapchain[0].bo.height() == h as u32 {
            return Ok(());
        }

        let (new0, new1) = {
            let ctx = self
                .gles_context
                .lock()
                .map_err(|_| MonitorSetupError::DrmError("Shared GLES context poisoned".into()))?;
            ctx.make_current_surfaceless()?;
            (
                create_slot(&ctx, &self.gl, &self.gbm_device, mode)?,
                create_slot(&ctx, &self.gl, &self.gbm_device, mode)?,
            )
        };

        if let Ok(ctx) = self.gles_context.lock() {
            let _ = ctx.make_current_surfaceless();
            for slot in &self.swapchain {
                if let Some(fb) = slot.drm_fb {
                    let _ = card.destroy_framebuffer(fb);
                }
                destroy_slot(&ctx, &self.gl, slot);
            }
        }

        self.swapchain = [new0, new1];
        self.current_slot = 0;
        Ok(())
    }

    pub fn context(&self) -> &T {
        &self.user_context
    }

    pub fn context_mut(&mut self) -> &mut T {
        &mut self.user_context
    }

    pub fn can_render(&self) -> bool {
        self.can_render
    }

    pub fn was_drawn(&self) -> bool {
        self.was_drawn
    }

    pub(crate) fn set_can_render(&mut self, value: bool) {
        self.can_render = value;
    }

    pub(crate) fn reset_drawn_flag(&mut self) {
        self.was_drawn = false;
    }

    pub fn make_current(&mut self) -> Result<(), GlesContextError> {
        let ctx = self
            .gles_context
            .lock()
            .map_err(|_| GlesContextError::MakeCurrentFailed)?;
        ctx.make_current_surfaceless()?;
        let slot = &self.swapchain[self.current_slot];
        unsafe {
            self.gl.BindFramebuffer(crate::gl::FRAMEBUFFER, slot.fbo);
            self.gl
                .Viewport(0, 0, slot.bo.width() as i32, slot.bo.height() as i32);
        }
        self.was_drawn = true;
        Ok(())
    }

    pub(crate) fn populate_commit(
        &mut self,
        card: &impl control::Device,
        atomic_req: &mut AtomicModeReq,
        in_fence_fd: Option<i32>,
    ) -> Result<(), MonitorSetupError> {
        let target_mode = self
            .requested_mode
            .as_ref()
            .unwrap_or(&self.default_mode)
            .clone();
        self.ensure_swapchain_for_mode(card, &target_mode)?;

        let ctx = self
            .gles_context
            .lock()
            .map_err(|_| MonitorSetupError::DrmError("Shared GLES context poisoned".into()))?;
        ctx.make_current_surfaceless()?;

        let slot = &mut self.swapchain[self.current_slot];
        let fb = if let Some(existing) = slot.drm_fb {
            existing
        } else {
            let flags = if slot.bo.modifier() == Modifier::Invalid {
                FbCmd2Flags::empty()
            } else {
                FbCmd2Flags::MODIFIERS
            };
            let created = card.add_planar_framebuffer(&slot.bo, flags).or_else(|_| card.add_framebuffer(&slot.bo, 24, 32)).map_err(|e| {
                MonitorSetupError::DrmError(format!("Failed to add framebuffer: {}", e))
            })?;
            slot.drm_fb = Some(created);
            created
        };

        let needs_mode_set = self.needs_mode_set();

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

        let (width, height) = target_mode.size();

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
        );

        if needs_mode_set {
            let mode_blob = card.create_property_blob(&target_mode).map_err(|e| {
                MonitorSetupError::DrmError(format!("Failed to create mode blob: {}", e))
            })?;
            atomic_req.add_property(
                self.current_crtc.handle(),
                self.crtc_properties["MODE_ID"].handle(),
                mode_blob,
            );
            atomic_req.add_property(
                self.current_crtc.handle(),
                self.crtc_properties["ACTIVE"].handle(),
                property::Value::Boolean(true),
            );
        }

        atomic_req.add_property(
            self.primary_plane_id,
            self.plane_properties["FB_ID"].handle(),
            property::Value::Framebuffer(Some(fb)),
        );

        if let Some(in_fence_fd) = in_fence_fd {
            // Reuse the same in-fence FD across all monitor properties in this commit.
            // The request carries FD numbers; the caller retains ownership and closes it.
            if let Some(fence_prop) = self.plane_properties.get("IN_FENCE_FD") {
                atomic_req.add_property(
                    self.primary_plane_id,
                    fence_prop.handle(),
                    property::Value::SignedRange(in_fence_fd as i64),
                );
            } else if let Some(fence_prop) = self.crtc_properties.get("IN_FENCE_FD") {
                atomic_req.add_property(
                    self.current_crtc.handle(),
                    fence_prop.handle(),
                    property::Value::SignedRange(in_fence_fd as i64),
                );
            }
        }

        self.current_slot = (self.current_slot + 1) % self.swapchain.len();

        self.first_frame = false;
        self.can_render = false;

        drop(ctx);
        if needs_mode_set {
            self.mark_mode_set();
        }

        Ok(())
    }


    pub(crate) fn needs_mode_set(&self) -> bool {
        self.requested_mode != self.current_mode
    }

    pub(crate) fn mark_mode_set(&mut self) {
        self.current_mode = self.requested_mode;
    }

    #[allow(dead_code)]
    pub(crate) fn clear_mode_state(&mut self) {
        self.current_mode = None;
        self.first_frame = true;
    }

    pub fn gl(&self) -> &crate::gl::Gles2 {
        &self.gl
    }

    pub fn get_proc_address(&self, symbol: &str) -> *const std::ffi::c_void {
        self.gles_context
            .lock()
            .map(|ctx| ctx.get_proc_address(symbol))
            .unwrap_or(std::ptr::null())
    }

    pub fn connector_id(&self) -> connector::Handle {
        self.connector_id
    }

    pub fn crtc(&self) -> &crtc::Info {
        &self.current_crtc
    }

    pub fn default_mode(&self) -> &control::Mode {
        &self.default_mode
    }

    pub fn current_mode(&self) -> Option<&control::Mode> {
        self.current_mode.as_ref()
    }

    pub fn requested_mode(&self) -> Option<&control::Mode> {
        self.requested_mode.as_ref()
    }

    pub fn set_mode(&mut self, mode: Option<control::Mode>) {
        self.requested_mode = mode;
    }

    pub fn active_mode(&self) -> &control::Mode {
        self.requested_mode.as_ref().unwrap_or(&self.default_mode)
    }

    pub fn primary_plane(&self) -> plane::Handle {
        self.primary_plane_id
    }

    pub fn cursor_plane(&self) -> Option<plane::Handle> {
        self.cursor_plane_id
    }

    pub fn size(&self) -> (u16, u16) {
        self.active_mode().size()
    }
}

impl<T> Drop for Monitor<T> {
    fn drop(&mut self) {
        if let Ok(ctx) = self.gles_context.lock() {
            let _ = ctx.make_current_surfaceless();
            for slot in &self.swapchain {
                destroy_slot(&ctx, &self.gl, slot);
            }
        }
    }
}
