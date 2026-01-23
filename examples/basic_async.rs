use std::time::Duration;

use easydrm::{gl, EasyDRM, MonitorContextCreationRequest};
use rand::Rng;
use tokio::time::Instant;

// Skia
use skia_safe::{self as skia, Font, FontMgr, Typeface, gpu::gl::FramebufferInfo};
use skia::gpu;

/// Per-monitor state (includes Skia GPU objects)
struct MonitorContext {
    // Visual demo state
    frame_count: u64,
    color_offset: f32,
    monitor_name: String,

    // FPS tracking
    fps_frames: u64,
    fps_last: Instant,
    fps_current: f64,

    // Skia GPU state (per monitor)
    gr: gpu::DirectContext,
    surface: skia::Surface,

    // Track size so we can recreate the surface if needed
    width: usize,
    height: usize,
    gl: easydrm::gl::Gles2,
    font: Typeface,
    fontmgr: FontMgr
}

impl MonitorContext {
    fn new(req: &MonitorContextCreationRequest<'_>) -> Self {
        // EasyDRM provides:
        // - req.gl: GLES2 bindings
        // - req.get_proc_address: loader for extra GL symbols
        // - req.width/height: current surface size

        unsafe {
            let version =
                std::ffi::CStr::from_ptr(req.gl.GetString(gl::VERSION) as *const i8);
            println!("Monitor initialized with OpenGL version: {:?}", version);
        }

        // Create Skia GL interface using the provided proc loader
        let interface = gpu::gl::Interface::new_load_with(req.get_proc_address)
        .expect("Failed to create Skia GL interface");

        let mut gr = gpu::direct_contexts::make_gl(interface, None)
            .expect("Failed to create Skia DirectContext");

        // Create a Skia surface that targets the currently bound framebuffer
        let surface = create_skia_surface_for_current_fbo(&mut gr, req.gl, req.width, req.height);

        let mut rng = rand::rng();
        let mut fontmgr = FontMgr::new();
        let font = fontmgr.match_family("Inter").new_typeface(0).unwrap();
        Self {
            fontmgr,
            font: font,
            frame_count: 0,
            color_offset: rng.random_range(0.0..1.0),
            monitor_name: format!("Monitor-{}", rng.random_range(0..10_000)),
            gl: req.gl.clone(),
            fps_frames: 0,
            fps_last: Instant::now(),
            fps_current: 0.0,

            gr,
            surface,

            width: req.width,
            height: req.height,
        }
    }

    fn update_and_get_color(&mut self) -> (f32, f32, f32) {
        self.frame_count += 1;
        self.fps_frames += 1;

        let hue = ((self.frame_count as f32 * 0.01 + self.color_offset) % 1.0).abs();
        hsv_to_rgb(hue, 0.8, 1.0)
    }

    fn maybe_update_fps(&mut self, now: Instant) {
        let elapsed = now.duration_since(self.fps_last);
        if elapsed >= Duration::from_secs(1) {
            self.fps_current = self.fps_frames as f64 / elapsed.as_secs_f64();
            self.fps_frames = 0;
            self.fps_last = now;
        }
    }

    fn ensure_surface(&mut self, width: usize, height: usize) {
        if self.width == width && self.height == height {
            return;
        }
        self.width = width;
        self.height = height;

        // GL context must be current when recreating this.
        self.surface = create_skia_surface_for_current_fbo(&mut self.gr, &self.gl, width, height);
    }

    fn draw_fps_overlay(&mut self) {
        let canvas = self.surface.canvas();

        // Small translucent background box
        let mut bg = skia::Paint::default();
        bg.set_anti_alias(true);
        bg.set_color(skia::Color::from_argb(160, 0, 0, 0));
        let rect = skia::Rect::from_xywh(12.0, 12.0, 200.0, 48.0);
        canvas.draw_round_rect(rect, 10.0, 10.0, &bg);

        // FPS text
        let mut paint = skia::Paint::default();
        paint.set_anti_alias(true);
        paint.set_color(skia::Color::WHITE);

        let font = skia::Font::from_typeface(&self.font, 26.);
        let text = format!("{:.1} FPS", self.fps_current);
        canvas.draw_str(text, (22.0, 46.0), &font, &paint);

        // Flush Skia -> GL
        self.gr.flush_and_submit();
    }

    fn status(&self) -> String {
        format!(
            "{} | fps={:.1} | total_frames={}",
            self.monitor_name,
            self.fps_current,
            self.frame_count
        )
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Initializing EasyDRM...");

    let mut easydrm = EasyDRM::init(MonitorContext::new)?;

    println!("EasyDRM initialized successfully!");
    println!("Found {} monitor(s)", easydrm.monitor_count());

    for (i, monitor) in easydrm.monitors().enumerate() {
        let mode = monitor.active_mode();
        let ctx = monitor.context();
        println!(
            "Monitor {}: {}x{} @ {}Hz - {}",
            i,
            mode.size().0,
            mode.size().1,
            mode.vrefresh(),
            ctx.monitor_name
        );
    }

    println!("\nStarting render loop. Press Ctrl+C to exit.");

    // ---------- Sentinel task ----------
    tokio::spawn(async {
        let mut ticker = tokio::time::interval(Duration::from_millis(2));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut last = Instant::now();
        let mut worst = Duration::ZERO;

        loop {
            ticker.tick().await;
            let now = Instant::now();
            let dt = now.duration_since(last);
            last = now;

            if dt > worst {
                worst = dt;
            }

            if dt > Duration::from_millis(10) {
                eprintln!(
                    "[SENTINEL] late tick: {:?} (worst={:?})",
                    dt, worst
                );
            }
        }
    });
    // ---------------------------------

    let mut global_frame_count: u64 = 0;
    let mut last_print = Instant::now();

    loop {
        let now = Instant::now();

        // Render per monitor
        for monitor in easydrm.monitors_mut() {
            if monitor.can_render() && monitor.make_current().is_ok() {

                // If modes can change, keep the Skia surface size synced with the monitor.
                // (If you prefer, you can use req.width/height at init and skip this.)
                let mode = monitor.active_mode();
                let (w, h) = (mode.size().0 as usize, mode.size().1 as usize);
                monitor.context_mut().ensure_surface(w, h);
                // Update per-monitor animation + FPS counter
                let (r, g, b) = monitor.context_mut().update_and_get_color();
                monitor.context_mut().maybe_update_fps(now);

                // GL clear
                unsafe {
                    let gl = monitor.gl();

                    gl.ClearColor(r, g, b, 1.0);
                    gl.Clear(gl::COLOR_BUFFER_BIT);
                }

                // Draw FPS overlay with Skia on top of the GL content
                monitor.context_mut().draw_fps_overlay();
            }
        }

        // Non-blocking atomic commit
        if let Err(e) = easydrm.swap_buffers() {
            eprintln!("Commit Error: {e:#?}")
        }

        // Wait for page-flip / hotplug events
        easydrm.poll_events_async().await?;

        global_frame_count += 1;

        // Print status once per second
        let now2 = Instant::now();
        if now2.duration_since(last_print) >= Duration::from_secs(1) {
            last_print = now2;

            println!(
                "=== status | global_frame={} | monitors={} ===",
                global_frame_count,
                easydrm.monitor_count()
            );
            for (i, monitor) in easydrm.monitors().enumerate() {
                println!("  Monitor {}: {}", i, monitor.context().status());
            }
            println!();
        }
    }
}

/// Create a Skia surface that renders into the currently bound GL framebuffer.
/// NOTE: The GL context for the target monitor must be current when calling this.
fn create_skia_surface_for_current_fbo(
    gr: &mut gpu::DirectContext,
    gl: &gl::Gles2,
    width: usize,
    height: usize,
) -> skia::Surface {
    // Query currently bound framebuffer
    let mut fbo: i32 = 0;
    unsafe {
        // If FRAMEBUFFER_BINDING is not available in your bindings,
        // try DRAW_FRAMEBUFFER_BINDING instead.
        gl.GetIntegerv(gl::FRAMEBUFFER_BINDING, &mut fbo);
    }

    let fb_info = FramebufferInfo {
        fboid: fbo as u32,
        // Common default. If your framebuffer format differs, match it here.
        format: gpu::gl::Format::RGBA8.into(),
        protected: gpu::Protected::No
    };

    let backend_rt = gpu::backend_render_targets::make_gl(
        (width as i32, height as i32),
        /* sample_cnt */ 0,
        /* stencil_bits */ 8,
        fb_info,
    );

    gpu::surfaces::wrap_backend_render_target(
        gr,
        &backend_rt,
        gpu::SurfaceOrigin::BottomLeft, // GL convention
        skia::ColorType::RGBA8888,
        None,
        None,
    )
    .expect("Failed to create Skia Surface from backend render target")
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let c = v * s;
    let h_prime = h * 6.0;
    let x = c * (1.0 - ((h_prime % 2.0) - 1.0).abs());
    let m = v - c;

    let (r, g, b) = match h_prime as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        5 => (c, 0.0, x),
        _ => (c, x, 0.0),
    };

    (r + m, g + m, b + m)
}
