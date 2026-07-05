# EasyDRM — Minimal DRM/KMS Framework

EasyDRM is a GLFW-inspired abstraction over DRM/KMS, GBM, and EGL/OpenGL that lets you build fullscreen Linux applications without a compositor (no X11, no Wayland). It owns the low-level plumbing—monitor discovery, events, page flips, fences, and atomic commits—so you can focus on your render loop while staying in total control of timing.

## Highlights

- **Single-threaded & explicit**: you drive the loop, EasyDRM provides blocking events and render surfaces.
- **Multi-monitor aware**: every monitor gets an isolated GL/EGL/GBM context, framebuffer, and fence.
- **Deterministic swap orchestration**: `swap_buffers()` walks the monitors you rendered and issues atomic commits in a predictable order.
- **Refresh-rate aware**: monitors are grouped by refresh rate for introspection and future scheduling tweaks.
- **3-state mode management**: `default_mode`, `requested_mode`, and `current_mode` minimize expensive modesets and handle TTY focus loss.
- **Robust fences**: GPU→DRM synchronization prevents tearing and leaks by cleaning sync objects every frame.

## Core Concept

```rust
use easydrm::EasyDRM;

let mut easydrm = EasyDRM::init_empty().expect("GPU available");

loop {
    for monitor in easydrm.monitors_mut() {
        if monitor.can_render() {     // 1) Draw only to ready monitors
            monitor.make_current().unwrap();
            render_frame(monitor);
        }
    }
    easydrm.swap_buffers().unwrap();  // 2) Commit each monitor that was drawn
    easydrm.poll_events().unwrap();   // 3) Block on DRM/input events

    // Alternatively you can also use the async version if you're using tokio
    // so it doesn't block everything and allows other concurrent futures to make progress
    // easydrm.poll_events_async().await.unwrap();   

    if easydrm.should_update() {      // 4) Global logic tied to fastest refresh group
        update_simulation();
    }
}
```

EasyDRM feels like a game engine main loop: you control the cadence, EasyDRM keeps hardware state in sync.

## Architecture Overview

### Event & Rendering Flow

1. `poll_events()` blocks on DRM, vblank, hotplug, and optional input fds via `poll()`.
2. `can_render()` turns true when the previous page flip + fence finished.
3. `make_current()` activates the monitor’s GL/EGL context so you can issue draw calls.
4. `swap_buffers()` iterates the monitors that were drawn and issues their atomic commits with the right fences.

### Multi-Monitor Grouping

- Monitors are grouped by refresh rate; the map is exposed so you can choose a cadence or diagnostics strategy.
- The helper `should_update()` fires once every time the fastest refresh-rate group has committed, letting you run simulation at that cadence.
- Every monitor tracks its own fence + framebuffer pair to keep scan-out safe.

### Mode System (Default / Requested / Current)

| Field            | Meaning                                  | When it changes                                        |
| ---------------- | ---------------------------------------- | ------------------------------------------------------ |
| `default_mode`   | Optimal mode detected at init            | Never after initialization                             |
| `requested_mode` | What the app wants (`None` = default)    | `monitor.set_mode(...)`                                |
| `current_mode`   | What DRM is actually using               | After successful atomic commit or `clear_mode_state()` |

A modeset runs when `requested_mode != current_mode`, covering first boot, TTY focus loss, and user-driven mode switches.


## Getting Started

### Prerequisites

- Linux environment with a DRM/KMS-capable GPU (running on a VT/TTY, not under X11/Wayland).
- Permissions to open `/dev/dri/card*` (run as root or add the user to the `video` group).
- Rust 1.84+ (edition 2024) and a modern Mesa/GBM/EGL stack.

### Build

```bash
cargo build --release
```

### Run the basic example

> ⚠️ Run from a VT (outside X/Wayland) to avoid fighting the system compositor.

```bash
cargo run --example basic
```

The example prints detected monitors, animates a color wipe, and keeps running until you Ctrl+C.

## API Sketch

- `EasyDRM::init_empty()` – initialize without a custom per-monitor context.
- `EasyDRM::init(|req| { /* create custom context using req.gl / req.get_proc_address */ })` – attach your own data per monitor.
- `EasyDRM::monitors()` / `monitors_mut()` – iterate over monitor handles.
- `Monitor::make_current()` – bind this monitor’s GL context and mark it as drawn.
- `Monitor::gl()` – access generated GLES2 bindings.
- `Monitor::set_mode(Some(mode))` – request a specific DRM mode; `None` reverts to `default_mode`.
- `EasyDRM::swap_buffers()` – walks monitors that were drawn and calls their atomic swap path.
- `EasyDRM::poll_events()` – wait for page flips, hotplug, and optional input events.
- `EasyDRM::should_update()` – returns true once per cycle when the fastest refresh-rate group has committed.

See `examples/basic.rs` and `examples/custom_context.rs` for end-to-end loops.

## Design Principles

- No automatic rendering—users decide exactly when and how to render.
- No UI toolkit logic—just buffers, events, and explicit synchronization.
- Works with any renderer (OpenGL, Skia, Clay, custom software) as long as it can target the provided GL context.
- Deterministic timing: a single `swap_buffers()` call per loop orchestrates every commit that needs to happen.

## Roadmap

- ✅ Global render loop & commit model
- ✅ Fence strategy plus refresh-rate grouping metadata
- ✅ 3-state display mode system
- ✅ Complete `Monitor::swap_buffers()` implementation
- 🚧 Cursor plane API
