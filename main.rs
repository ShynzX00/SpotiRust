// src/main.rs
//
// Spoti-Rust entry point.
//
// Module layout
// ─────────────────────────────────────────────────────────────────────────────
//   cef/       — CEF init, browser creation, JS injection, IPC handler
//   ipc/       — Strongly-typed JS ↔ Rust message contract
//   player/    — Arc<Mutex<PlayerState>> + CommandSink trait
//   controls/  — souvlaki (MPRIS / Now Playing / SMTC)
//   tray/      — tray-icon right-click menu
//   widevine   — platform-aware Widevine CDM path resolution
//
// Thread model
// ─────────────────────────────────────────────────────────────────────────────
//   Main thread  : CEF message loop + tray event polling (required by both)
//   CEF internal : libcef spawns its own IO / GPU threads
//   Tokio pool   : available for future async work (not yet used)

mod cef;        // our cef module, not the cef crate
mod controls;
mod ipc;
mod player;
mod tray;
mod widevine;

use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::{
    ipc::JsCommand,                    // ← imported from crate::ipc, not player
    player::{new_shared_state, CommandSink, SharedState},
};

// ── CommandSink backed by a live cef::Browser ─────────────────────────────────

struct CefCommandSink {
    browser: parking_lot::Mutex<Option<::cef::Browser>>,
}

impl CefCommandSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            browser: parking_lot::Mutex::new(None),
        })
    }

    fn set_browser(&self, b: ::cef::Browser) {
        *self.browser.lock() = Some(b);
    }
}

impl CommandSink for CefCommandSink {
    fn send(&self, cmd: JsCommand) {
        if let Some(ref browser) = *self.browser.lock() {
            cef::exec_js(browser, cmd);
        } else {
            tracing::warn!("CommandSink: browser not ready, dropping {cmd:?}");
        }
    }
}

// ── Drop guard for clean CEF shutdown ────────────────────────────────────────

struct CefGuard;
impl Drop for CefGuard {
    fn drop(&mut self) {
        info!("Shutting down CEF");
        cef::shutdown();
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    // ── Logging ───────────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("spoti-rust starting");

    // ── CEF subprocess re-entry check ─────────────────────────────────────────
    // libcef re-executes this binary for renderer/GPU/utility sub-processes.
    // Those must return early before any application-level initialisation.
    let exit_code = cef::run_subprocess();
    if exit_code >= 0 {
        std::process::exit(exit_code);
    }

    // ── Shared player state ───────────────────────────────────────────────────
    let state: SharedState = new_shared_state();

    // ── CEF init ──────────────────────────────────────────────────────────────
    if !cef::init() {
        eprintln!("Fatal: CEF failed to initialise");
        std::process::exit(1);
    }
    let _cef_guard = CefGuard; // calls cef::shutdown() on drop

    // ── Command sink ──────────────────────────────────────────────────────────
    let sink = CefCommandSink::new();
    let sink_dyn: Arc<dyn CommandSink> = Arc::clone(&sink) as Arc<dyn CommandSink>;

    // ── OS media controls (souvlaki) ──────────────────────────────────────────
    let mut media_controls =
        controls::init_media_controls(Arc::clone(&state), Arc::clone(&sink_dyn))
            .expect("Failed to initialise OS media controls");

    // ── System tray ───────────────────────────────────────────────────────────
    let tray = tray::TrayManager::new(Arc::clone(&state), Arc::clone(&sink_dyn));

    // ── CEF browser ───────────────────────────────────────────────────────────
    let client = cef::SpotiClient::new(Arc::clone(&state), Arc::clone(&sink_dyn));
    if let Some(browser) = cef::create_browser(client) {
        sink.set_browser(browser);
    } else {
        eprintln!("Fatal: could not create CEF browser");
        std::process::exit(1);
    }

    // ── TODO: UI window hook ──────────────────────────────────────────────────
    // Create your lightweight native UI window here (winit + wgpu, or a plain
    // Win32 / Cocoa / GTK window).
    //
    //   let ui = ui::Window::new(Arc::clone(&state), Arc::clone(&sink_dyn));
    //   ui.show();
    //
    // The window should:
    //   • Read from `state` to draw the now-playing view.
    //   • Call `sink_dyn.send(JsCommand::TogglePlay)` on button clicks.
    //   • On close: hide itself instead of calling std::process::exit().
    //
    // On Windows, also pass the HWND to `controls::platform_config()` after
    // the window is created so SMTC binds to the right handle.
    // ─────────────────────────────────────────────────────────────────────────

    info!("Entering main event loop (~60 Hz)");

    // ── Main event loop ───────────────────────────────────────────────────────
    // Both CEF and tray-icon require their event processing to happen on the
    // main OS thread, so we merge them into a single spin loop.
    //
    // Using tokio::time::sleep keeps the loop async-runtime-friendly for when
    // you wire in a tokio channel for state-change notifications.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("tokio runtime");

    rt.block_on(async {
        loop {
            // Drain one CEF message loop tick.
            cef::pump();

            // Drain tray / menu events.
            tray.poll();

            // Push updated metadata to the OS Now Playing / MPRIS / SMTC bar.
            controls::update_metadata(&mut media_controls, &state);

            // TODO: UI window hook – tick the UI event loop here, e.g.:
            //   ui.poll_events();

            // ~60 Hz — avoids burning a full CPU core on the main thread.
            sleep(Duration::from_millis(16)).await;
        }
    });
}
