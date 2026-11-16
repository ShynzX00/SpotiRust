// src/cef/mod.rs
//
// CEF lifecycle — built against tauri-apps/cef-rs (cef = "156").
//
// Real API surface (verified from docs.rs/cef):
//
//   cef::execute_process(Option<&MainArgs>, Option<&mut App>, *mut u8) -> c_int
//   cef::initialize(Option<&MainArgs>, Option<&Settings>, Option<&mut App>, *mut u8) -> c_int
//   cef::do_message_loop_work()
//   cef::shutdown()
//
//   cef::args::Args::new()  →  .as_main_args() → &MainArgs
//
//   cef::Settings  — plain struct with Default; fields are CefString / c_int.
//                    No `widevine_cdm_path` field; Widevine is loaded via the
//                    --widevine-cdm-path command-line switch instead.
//
//   wrap_app!(SpotiApp)    — generates App wrapper + WrapApp impl
//   wrap_client!(SpotiClient) — generates Client wrapper + WrapClient impl
//
// The IPC channel from JS → Rust goes through CEF's ProcessMessage mechanism.
// The bridge exposes `window.cefQuery(...)` on the renderer side; on the
// browser side we receive it in `on_process_message_received`.

use std::sync::Arc;
use parking_lot::Mutex;
use tracing::{debug, error, info, warn};

use cef::{
    args::Args,
    string::CefString,
    Browser, BrowserSettings, Frame, LogSeverity, ProcessMessage, Settings,
    WindowInfo,
};

use crate::{
    ipc::{InboundMessage, JsCommand},
    player::{CommandSink, SharedState},
    widevine::widevine_cdm_path,
};

// ── Bridge JS (embedded at compile time) ─────────────────────────────────────

const BRIDGE_JS: &str = include_str!("../../assets/bridge.js");

// ── Settings ──────────────────────────────────────────────────────────────────

/// Returns the user-data directory used for persistent CEF cookies + cache.
pub fn user_data_path() -> std::path::PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("spoti-rust")
        .join("cef-profile")
}

/// Build a `cef::Settings` value.
///
/// `Settings` has `Default` so we only override what we need.
pub fn build_settings() -> Settings {
    let profile = user_data_path();
    std::fs::create_dir_all(&profile).ok();

    let profile_str = profile.to_str().unwrap_or("").to_owned();
    let cache_str   = profile.join("cache").to_str().unwrap_or("").to_owned();

    Settings {
        // Persistent login — cookies survive restarts.
        root_cache_path:       CefString::from(profile_str.as_str()),
        cache_path:            CefString::from(cache_str.as_str()),
        persist_session_cookies: 1,

        // Suppress the sandbox so we don't need a setuid helper.
        no_sandbox: 1,

        // Remote DevTools on chrome://inspect → localhost:9222 during dev.
        remote_debugging_port: 9222,

        log_severity: LogSeverity::LOGSEVERITY_WARNING,

        // Let CEF drive its own message loop; we call do_message_loop_work()
        // manually in the main thread.
        external_message_pump: 1,

        ..Settings::default()
    }
}

// ── App wrapper ───────────────────────────────────────────────────────────────
//
// `wrap_app!` generates:
//   • A `SpotiApp` struct that wraps `cef::App`.
//   • `WrapApp for SpotiApp` and the required ref-counted boilerplate.
//
// We override `on_before_command_line_processing` to inject Chromium flags
// (Widevine CDM path, autoplay policy, codec support).

cef::wrap_app!(SpotiApp);

impl cef::ImplApp for SpotiApp {
    fn on_before_command_line_processing(
        &self,
        process_type: Option<&CefString>,
        command_line: Option<cef::CommandLine>,
    ) {
        let Some(mut cl) = command_line else { return };

        // Only apply these switches in the browser process.
        if process_type.map(|s| !s.to_string().is_empty()).unwrap_or(false) {
            return;
        }

        // Proprietary audio/video codecs (AAC, MP3) required by Spotify.
        cl.append_switch(Some(&CefString::from("enable-proprietary-codecs")));

        // Allow Spotify's web player to autoplay without a user gesture.
        cl.append_switch_with_value(
            Some(&CefString::from("autoplay-policy")),
            Some(&CefString::from("no-user-gesture-required")),
        );

        // Widevine CDM — loaded via command-line switch because cef::Settings
        // has no widevine_cdm_path field in this version.
        if let Some(wv) = widevine_cdm_path() {
            if let Some(wv_str) = wv.to_str() {
                info!("Widevine CDM: {wv_str}");
                cl.append_switch_with_value(
                    Some(&CefString::from("widevine-cdm-path")),
                    Some(&CefString::from(wv_str)),
                );
            }
        } else {
            warn!("Widevine CDM not found – DRM content will not play");
        }

        // Platform-specific GPU flags.
        #[cfg(target_os = "linux")]
        {
            cl.append_switch(Some(&CefString::from("enable-gpu-rasterization")));
            cl.append_switch_with_value(
                Some(&CefString::from("use-gl")),
                Some(&CefString::from("desktop")),
            );
        }

        #[cfg(target_os = "macos")]
        {
            cl.append_switch_with_value(
                Some(&CefString::from("enable-features")),
                Some(&CefString::from("MetalANGLE")),
            );
        }
    }
}

// ── Client (browser handler) ──────────────────────────────────────────────────
//
// `wrap_client!` generates the `Client` wrapper + `WrapClient` impl.
// We store our shared state and command sink inside `SpotiClient`.

pub struct SpotiClient {
    state: SharedState,
    sink:  Arc<dyn CommandSink>,
}

impl SpotiClient {
    pub fn new(state: SharedState, sink: Arc<dyn CommandSink>) -> Self {
        Self { state, sink }
    }
}

cef::wrap_client!(SpotiClient);

impl cef::ImplClient for SpotiClient {
    // ── Load handler: inject bridge.js on Spotify frames ─────────────────────

    fn get_load_handler(&self) -> Option<cef::LoadHandler> {
        // Return None here; we implement on_load_end via ImplLoadHandler below.
        // For simplicity we attach load callbacks through get_load_handler
        // returning a separate wrapped handler. In a minimal build you can
        // also just handle the callback inline here by returning a boxed impl.
        None
        // TODO: return Some(LoadHandler::wrap(SpotiLoadHandler { ... }))
        //       and implement ImplLoadHandler for SpotiLoadHandler.
    }

    // ── IPC: JS → Rust ────────────────────────────────────────────────────────

    fn on_process_message_received(
        &self,
        _browser: Browser,
        _frame:   Option<Frame>,
        _source:  cef::ProcessId,
        message:  Option<ProcessMessage>,
    ) -> bool {
        let Some(msg) = message else { return false };

        // The bridge sends a single-string argument containing JSON.
        let Some(args) = msg.get_argument_list() else { return false };
        let Some(json_str) = args.get_string(0) else { return false };
        let json = json_str.to_string();

        match serde_json::from_str::<InboundMessage>(&json) {
            Ok(InboundMessage::StateUpdate(update)) => {
                debug!("StateUpdate: {} – playing={}", update.title, update.is_playing);
                self.state.lock().current = Some(update);
                // TODO: UI window hook – notify the frontend thread here.
            }
            Ok(InboundMessage::PlayerReady(ready)) => {
                info!("PlayerReady: authenticated={}", ready.authenticated);
                self.state.lock().authenticated = ready.authenticated;
            }
            Err(e) => {
                error!("IPC parse error: {e}  raw={json}");
            }
        }

        true // consumed
    }
}

// ── CEF subprocess re-entry ───────────────────────────────────────────────────

/// Must be called at the very top of `main()`, before any other init.
///
/// CEF spawns renderer / GPU / utility sub-processes by re-executing this
/// binary with a `--type=...` flag.  `execute_process` detects that and
/// runs the sub-process, returning `≥ 0`.  For the browser process it
/// returns `-1` and we continue normally.
pub fn run_subprocess() -> i32 {
    let args = Args::new();
    // We need a temporary SpotiApp for sub-processes too.
    let mut app = SpotiApp::new();
    unsafe {
        cef::execute_process(
            Some(args.as_main_args()),
            Some(&mut app.as_mut_app()),
            std::ptr::null_mut(),
        )
    }
}

// ── CEF initialisation ────────────────────────────────────────────────────────

/// Initialise the CEF browser process.  Returns `true` on success.
pub fn init() -> bool {
    let args     = Args::new();
    let settings = build_settings();
    let mut app  = SpotiApp::new();

    let result = unsafe {
        cef::initialize(
            Some(args.as_main_args()),
            Some(&settings),
            Some(&mut app.as_mut_app()),
            std::ptr::null_mut(),
        )
    };

    if result != 1 {
        error!("cef::initialize returned {result} (expected 1)");
        return false;
    }

    info!("CEF initialised (profile: {})", user_data_path().display());
    true
}

// ── Browser creation ──────────────────────────────────────────────────────────

/// Create the hidden Chromium window that loads open.spotify.com.
///
/// Returns the `cef::Browser` handle so callers can inject JS commands.
pub fn create_browser(client: SpotiClient) -> Option<Browser> {
    let mut window_info = WindowInfo::default();

    // Keep the window hidden — audio plays without visible Chromium chrome.
    #[cfg(target_os = "linux")]
    {
        window_info.window_name = CefString::from("spoti-rust-hidden");
        // On Linux, setting width/height to 0 and not setting a parent window
        // creates an off-screen / windowless-equivalent window.
    }
    #[cfg(target_os = "windows")]
    {
        window_info.window_name = CefString::from("spoti-rust-hidden");
        window_info.style       = cef::sys::WS_OVERLAPPEDWINDOW;
        // Hide immediately after creation via LifeSpanHandler::on_after_created.
    }
    #[cfg(target_os = "macos")]
    {
        window_info.window_name = CefString::from("spoti-rust-hidden");
    }

    let browser_settings = BrowserSettings::default();

    cef::browser_host::create_browser_sync(
        Some(&window_info),
        Some(client.into_client()),   // SpotiClient → cef::Client
        Some(&CefString::from("https://open.spotify.com")),
        Some(&browser_settings),
        None,  // extra_info
        None,  // request_context
    )
}

// ── Command execution ─────────────────────────────────────────────────────────

/// Evaluate a JS command in the Spotify frame.
///
/// Called from the `CommandSink` impl in `main.rs` and from the controls /
/// tray modules whenever a media key or menu item fires.
pub fn exec_js(browser: &Browser, cmd: JsCommand) {
    let js = cmd.to_js();
    if let Some(frame) = browser.get_main_frame() {
        frame.execute_java_script(
            Some(&CefString::from(js.as_str())),
            Some(&CefString::from("spoti-rust://cmd")),
            0,
        );
        debug!("→ JS: {js}");
    }
}

/// Inject bridge.js into a frame.  Called from a `LoadHandler::on_load_end`
/// implementation when the frame URL contains "open.spotify.com".
pub fn inject_bridge(frame: &Frame, url: &str) {
    if url.contains("open.spotify.com") {
        info!("Injecting bridge.js into {url}");
        frame.execute_java_script(
            Some(&CefString::from(BRIDGE_JS)),
            Some(&CefString::from(url)),
            0,
        );
    }
}

// ── Message loop helpers ──────────────────────────────────────────────────────

/// Run one iteration of the CEF message loop.
/// Must be called from the main thread.
pub fn pump() {
    cef::do_message_loop_work();
}

/// Shut CEF down cleanly.  Called via a Drop guard in `main.rs`.
pub fn shutdown() {
    cef::shutdown();
}
