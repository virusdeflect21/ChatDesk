#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! ChatDesk Web — a wry-based Google Chat shell for Windows.
//!
//! Loads chat.google.com directly in a wry webview, keeps a tray icon with an
//! unread badge, surfaces page notifications as tray balloons, and enforces a
//! single-instance lock. Everything that can live in Rust does.

use std::{
    cell::RefCell,
    ffi::c_void,
    fs,
    net::{TcpListener, TcpStream},
    path::PathBuf,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant, SystemTime},
};

include!(concat!(env!("OUT_DIR"), "/icon_data.rs"));

use anyhow::{Context, Result};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    platform::windows::WindowExtWindows,
    window::{Icon, Window, WindowBuilder},
};
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
};
use wry::{
    MemoryUsageLevel, NewWindowResponse, PageLoadEvent, PermissionKind, PermissionResponse, Theme,
    WebContext, WebView, WebViewBuilder, WebViewBuilderExtWindows, WebViewExtWindows,
};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::HICON;

const APP_NAME: &str = "ChatDesk Web";
const START_URL: &str = "https://chat.google.com";
const WINDOW_TITLE: &str = "Google Chat";
const PROFILE_DIR: &str = "ChatDeskWeb";
const SETTINGS_FILE: &str = "settings.txt";
const RUN_VALUE_NAME: &str = "ChatDesk";
const SINGLE_INSTANCE_ADDR: &str = "127.0.0.1:39147";
const MIN_ZOOM: f64 = 0.5;
const MAX_ZOOM: f64 = 2.0;

/// Everything ChatDesk remembers between launches: the autoboot and
/// start-hidden switches, the zoom level and the window geometry. Stored as
/// plain `key=value` lines so it stays human-readable and dependency-free.
#[derive(Clone)]
struct Settings {
    launch_at_login: bool,
    start_hidden: bool,
    zoom: f64,
    win_x: i32,
    win_y: i32,
    win_w: u32,
    win_h: u32,
    maximized: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            launch_at_login: false,
            start_hidden: false,
            zoom: 1.0,
            win_x: i32::MIN,
            win_y: i32::MIN,
            win_w: 0,
            win_h: 0,
            maximized: false,
        }
    }
}

impl Settings {
    fn load() -> Self {
        let mut settings = Self::default();
        let Ok(text) = fs::read_to_string(settings_path()) else {
            return settings;
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "launch_at_login" => settings.launch_at_login = value == "1",
                "start_hidden" => settings.start_hidden = value == "1",
                "zoom" => {
                    if let Ok(zoom) = value.parse::<f64>() {
                        settings.zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
                    }
                }
                "win_x" => settings.win_x = value.parse().unwrap_or(i32::MIN),
                "win_y" => settings.win_y = value.parse().unwrap_or(i32::MIN),
                "win_w" => settings.win_w = value.parse().unwrap_or(0),
                "win_h" => settings.win_h = value.parse().unwrap_or(0),
                "maximized" => settings.maximized = value == "1",
                _ => {}
            }
        }
        settings
    }

    fn save(&self) {
        let text = format!(
            "launch_at_login={}\nstart_hidden={}\nzoom={}\nwin_x={}\nwin_y={}\nwin_w={}\nwin_h={}\nmaximized={}\n",
            self.launch_at_login as u8,
            self.start_hidden as u8,
            self.zoom,
            self.win_x,
            self.win_y,
            self.win_w,
            self.win_h,
            self.maximized as u8,
        );
        if let Some(dir) = settings_path().parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = fs::write(settings_path(), text);
    }
}

fn settings_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|root| root.join(PROFILE_DIR).join(SETTINGS_FILE))
        .unwrap_or_else(|| PathBuf::from(SETTINGS_FILE))
}

/// The `HKCU\...\Run` entry that makes Windows start ChatDesk at login.
/// The settings file is the source of truth; the registry is synced to it on
/// every launch and whenever the tray switch is flipped.
fn set_launch_at_login(on: bool) {
    use windows::core::{w, HSTRING};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyW, RegDeleteValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ,
    };

    let wide = |value: &str| -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    };

    unsafe {
        let mut key = HKEY::default();
        if on {
            let Ok(exe) = std::env::current_exe() else {
                return;
            };
            let command = format!("\"{}\"", exe.display());
            if RegCreateKeyW(
                HKEY_CURRENT_USER,
                w!("Software\\\\Microsoft\\\\Windows\\\\CurrentVersion\\\\Run"),
                &mut key,
            )
            .is_ok()
            {
                let bytes = wide(&command);
                let bytes = std::slice::from_raw_parts(bytes.as_ptr() as *const u8, bytes.len() * 2);
                let _ = RegSetValueExW(
                    key,
                    &HSTRING::from(RUN_VALUE_NAME),
                    None,
                    REG_SZ,
                    Some(bytes),
                );
                let _ = RegCloseKey(key);
            }
        } else if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\\\Microsoft\\\\Windows\\\\CurrentVersion\\\\Run"),
            None,
            KEY_SET_VALUE,
            &mut key,
        )
        .is_ok()
        {
            let _ = RegDeleteValueW(key, &HSTRING::from(RUN_VALUE_NAME));
            let _ = RegCloseKey(key);
        }
    }
}

/// True once the chat page has finished loading.
static CHAT_LOADED: AtomicBool = AtomicBool::new(false);
/// True while the window is in the foreground, so we never balloon messages the
/// user is already looking at.
static FOCUSED: AtomicBool = AtomicBool::new(false);

/// Chromium flags tuned for the smallest footprint that still runs Google Chat.
///
/// * One renderer, no site isolation and no spare renderer: chat.google.com
///   embeds several cross-origin frames, and with isolation every one of them
///   becomes its own ~80 MB renderer process.
/// * GPU and network services run inside the browser process instead of
///   separate utility processes.
/// * Background throttling is left ON so a minimized window gets cheap.
/// * wry's default flags (mini menu / SmartScreen off) are replaced by
///   anything passed here, so they are repeated in the one --disable-features.
const BROWSER_ARGS: &str = "--no-first-run \
--no-default-browser-check \
--in-process-gpu \
--process-per-site \
--renderer-process-limit=1 \
--disable-site-isolation-trials \
--enable-low-end-device-mode \
--disable-background-networking \
--disable-component-update \
--disable-breakpad \
--disable-extensions \
--disable-sync \
--disable-translate \
--disable-default-apps \
--disable-domain-reliability \
--disable-client-side-phishing-detection \
--disable-gpu-shader-disk-cache \
--metrics-recording-only \
--no-pings \
--autoplay-policy=no-user-gesture-required \
--disk-cache-size=16777216 \
--media-cache-size=4194304 \
--enable-features=NetworkServiceInProcess,NetworkServiceInProcess2 \
--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,site-per-process,IsolateOrigins,\
SpareRendererForSitePerProcess,BackForwardCache,OptimizationHints,OptimizationGuideModelDownloading,\
MediaRouter,Translate,AutofillServerCommunication,InterestFeedContentSuggestions,\
CertificateTransparencyComponentUpdater,msEdgeShoppingUI,msEdgeCollections,msImplicitSignin,\
msDiagnosticDataForceEnabled,msAutofillEdgeServiceCommunication,WebRtcAllowInputVolumeAdjustment";

/// V8 tuning. `--lite-mode` runs JS in the interpreter only (no optimizing
/// JIT, no feedback vectors): the chat renderer drops ~20-50 MB, and Google
/// Chat stays responsive because it is mostly idle. `CHATDESK_WEB_FULL_JIT=1`
/// turns the JIT back on for maximum JS speed.
const JS_FLAGS: &str =
    "--optimize-for-size,--max-old-space-size=128,--max-semi-space-size=1,--stack-size=1048576";

fn browser_args() -> String {
    let full_jit = std::env::var_os("CHATDESK_WEB_FULL_JIT").is_some_and(|v| v != "0");
    let lite = if full_jit { "" } else { "--lite-mode," };
    let mut args = format!("{BROWSER_ARGS} --js-flags={lite}{JS_FLAGS}");
    // Opt-in software rendering: no GPU context at all, a little less RAM,
    // at the cost of scroll smoothness.
    if std::env::var_os("CHATDESK_WEB_SOFTWARE_RENDERING").is_some_and(|v| v != "0") {
        args.push_str(" --disable-gpu --disable-gpu-compositing");
    }
    args
}

/// Passive (tray) mode keeps the page running so messages still raise tray
/// balloons, but pushes the whole WebView2 tree's memory back to Windows right
/// away and again every `HIDDEN_TRIM_SECS` while hidden.
///
/// `CHATDESK_WEB_DEEP_SLEEP=1` instead shuts the engine down entirely after
/// `CHATDESK_WEB_TEARDOWN_SECS` (default 30) in the tray: ~3 MB, but no
/// messages until the window is reopened.
const DEFAULT_TEARDOWN_SECS: u64 = 30;
const HIDDEN_TRIM_SECS: u64 = 30;

/// Google Chat re-raises notifications for already-pending messages right
/// after it loads. Anything arriving this soon after a load is not ballooned.
const NOTIFY_GRACE_SECS: u64 = 20;

/// How long the window may sit unfocused before the engine is asked to drop
/// caches (MemoryUsageLevel::Low).
const IDLE_TRIM_SECS: u64 = 20;

const EDGE_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36 Edg/157.0.0.0";

/// The only JavaScript this app injects. It forwards the badge count and, as a
/// fallback, page-raised notifications. The native WebView2 notification hook
/// is the primary path.
const PAGE_BRIDGE: &str = r#"
(function () {
    var post = function (s) { try { window.ipc.postMessage(s); } catch (e) {} };
    var clean = function (v) {
        return String(v == null ? '' : v).replace(/[\t\r\n]+/g, ' ').slice(0, 200);
    };
    var notify = function (title, options) {
        post('notify\t' + clean(title) + '\t' + clean(options && options.body));
    };

    // Page-raised notifications (fallback when the native hook is unavailable).
    try {
        var Orig = window.Notification;
        if (Orig) {
            var Patched = function (title, options) {
                notify(title, options);
                return new Orig(title, options);
            };
            Patched.requestPermission = Orig.requestPermission
                ? Orig.requestPermission.bind(Orig)
                : function () { return Promise.resolve('granted'); };
            try {
                Object.defineProperty(Patched, 'permission', {
                    get: function () { return Orig.permission; }
                });
            } catch (e) {}
            Patched.maxActions = Orig.maxActions;
            window.Notification = Patched;
        }
    } catch (e) {}

    // Service-worker notifications (fallback).
    try {
        var SWR = window.ServiceWorkerRegistration;
        if (SWR && SWR.prototype && SWR.prototype.showNotification) {
            var origShow = SWR.prototype.showNotification;
            SWR.prototype.showNotification = function (title, options) {
                notify(title, options);
                return origShow.apply(this, arguments);
            };
        }
    } catch (e) {}

    // The standard badging API.
    try {
        if (navigator.setAppBadge) {
            var setAppBadge = navigator.setAppBadge.bind(navigator);
            navigator.setAppBadge = function (count) {
                post('unread:' + (count == null ? 1 : count));
                return setAppBadge.apply(navigator, arguments);
            };
        }
    } catch (e) {}

    document.addEventListener('DOMContentLoaded', function () {
        var style = document.createElement('style');
        style.textContent = 'html, body { background: #202124; } html { overscroll-behavior: none; }';
        document.head.appendChild(style);
    });
})();
"#;

#[derive(Debug)]
enum UserEvent {
    Show,
    OpenChat,
    Badge(bool),
    Notify { title: String, body: String },
    ChatLoaded,
    SelfTest,
    Quit,
    Nav(&'static str),
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleLaunchAtLogin,
    ToggleStartHidden,
    CheckUpdates,
    ResetAppData,
}

fn main() -> Result<()> {
    let started = Instant::now();

    set_app_user_model_id();

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Claim the single-instance port before doing any expensive work. If it is
    // taken, this launch just pokes the running instance and exits.
    let Some(listener) = claim_single_instance() else {
        return Ok(());
    };

    let settings = Settings::load();
    set_launch_at_login(settings.launch_at_login);

    let profile = profile_dir()?;
    fs::create_dir_all(&profile)?;
    register_app_identity();

    let icon = app_icon();
    let window = WindowBuilder::new()
        .with_title(WINDOW_TITLE)
        .with_inner_size(LogicalSize::new(1180.0, 820.0))
        .with_min_inner_size(LogicalSize::new(760.0, 520.0))
        .with_window_icon(Some(icon))
        .with_decorations(true)
        .build(&event_loop)
        .context("cannot create the application window")?;

    if settings.win_w > 0 && settings.win_h > 0 {
        window.set_inner_size(tao::dpi::PhysicalSize::new(settings.win_w, settings.win_h));
    }
    if settings.win_x != i32::MIN && settings.win_y != i32::MIN {
        window.set_outer_position(tao::dpi::PhysicalPosition::new(settings.win_x, settings.win_y));
    }
    if settings.maximized {
        window.set_maximized(true);
    }

    let hwnd = HWND(window.hwnd() as *mut c_void);

    let normal_tray_icon = tray_icon_image()?;
    let badge_tray_icon = tray_icon::Icon::from_rgba(
        ICON_BADGE_32.to_vec(),
        ICON_SIZE,
        ICON_SIZE,
    )
    .context("invalid badge icon buffer")?;
    let overlay_on = rgba_to_hicon(ICON_OVERLAY_16, OVERLAY_SIZE);

    install_tray_handlers(&proxy);

    let tray = Rc::new(RefCell::new(None));
    let (menu, menu_handles) = build_menu(&settings)?;
    *tray.borrow_mut() = Some(
        TrayIconBuilder::new()
            .with_icon(normal_tray_icon.clone())
            .with_tooltip(format!("{APP_NAME} — Google Chat"))
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?,
    );

    let mut engine = Engine {
        context: WebContext::new(Some(profile)),
        proxy: proxy.clone(),
        downloads: downloads_dir(),
        loaded_marker: marker_path("CHATDESK_WEB_LOADED_MARKER"),
        started,
        zoom: settings.zoom,
    };

    let webview = match engine.build(&window) {
        Ok(webview) => webview,
        Err(err) => {
            fatal(&format!(
                "Microsoft Edge WebView2 could not start.\n\n\
                 Windows 10/11 normally ships the Evergreen Runtime. Install it from \
                 https://developer.microsoft.com/microsoft-edge/webview2/ and start ChatDesk again.\n\n\
                 Details: {err}"
            ));
            return Ok(());
        }
    };

    let listener_proxy = proxy.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = listener_proxy.send_event(UserEvent::Show);
            drop(stream);
        }
    });

    let start_hidden = settings.start_hidden;
    run(
        event_loop,
        window,
        engine,
        webview,
        tray,
        hwnd,
        overlay_on,
        normal_tray_icon,
        badge_tray_icon,
        started,
        Rc::new(RefCell::new(settings)),
        menu_handles,
        start_hidden,
        Timings {
            window: marker_path("CHATDESK_WEB_WINDOW_MARKER"),
            badge: marker_path("CHATDESK_WEB_BADGE_MARKER"),
            notify: marker_path("CHATDESK_WEB_NOTIFY_MARKER"),
        },
    );

    #[allow(unreachable_code)]
    Ok(())
}

/// Everything needed to (re)create the webview. The engine is shut down while
/// the app sits in the tray and rebuilt from this on demand; the profile on
/// disk keeps the session, so no new sign-in is needed.
struct Engine {
    context: WebContext,
    proxy: EventLoopProxy<UserEvent>,
    downloads: PathBuf,
    loaded_marker: Option<PathBuf>,
    started: Instant,
    zoom: f64,
}

impl Engine {
    fn build(&mut self, window: &Window) -> wry::Result<WebView> {
        let proxy_for_ipc = self.proxy.clone();
        let proxy_for_title = self.proxy.clone();
        let proxy_for_load = self.proxy.clone();
        let downloads = self.downloads.clone();
        let loaded_marker = self.loaded_marker.clone();
        let started = self.started;

        let webview = WebViewBuilder::new_with_web_context(&mut self.context)
            .with_url(start_url())
            .with_user_agent(EDGE_UA)
            .with_theme(Theme::Dark)
            .with_background_color((32, 33, 36, 255))
            .with_devtools(false)
            .with_hotkeys_zoom(false)
            .with_browser_accelerator_keys(false)
            .with_additional_browser_args(browser_args())
            .with_permission_handler(|kind| match kind {
                PermissionKind::Notifications
                | PermissionKind::ClipboardRead
                | PermissionKind::Microphone
                | PermissionKind::Camera => PermissionResponse::Allow,
                _ => PermissionResponse::Default,
            })
            .with_navigation_handler(|url| {
                if nav_allowed(&url) {
                    true
                } else if !is_google(&url) {
                    let _ = open::that(&url);
                    false
                } else {
                    false
                }
            })
            .with_new_window_req_handler(|url, _| {
                if !is_google(&url) {
                    let _ = open::that(&url);
                }
                if !CHAT_LOADED.load(Ordering::Acquire) && host_of(&url) == "accounts.google.com" {
                    NewWindowResponse::Allow
                } else {
                    NewWindowResponse::Deny
                }
            })
            .with_download_started_handler(move |url, path| {
                if let Some(name) = download_name(&url) {
                    *path = downloads.join(name);
                }
                true
            })
            // Unread count straight from the document title, parsed in Rust.
            .with_document_title_changed_handler(move |title| {
                if parse_unread(&title) > 0 && !FOCUSED.load(Ordering::Relaxed) {
                    let _ = proxy_for_title.send_event(UserEvent::Badge(true));
                }
            })
            .with_ipc_handler(move |request| {
                let body = request.body();
                if let Some(rest) = body.strip_prefix("notify\t") {
                    let mut parts = rest.splitn(2, '\t');
                    let title = parts.next().unwrap_or_default().to_string();
                    let text = parts.next().unwrap_or_default().to_string();
                    let _ = proxy_for_ipc.send_event(UserEvent::Notify { title, body: text });
                } else if let Some(count) = body.strip_prefix("unread:") {
                    if count != "0" && !FOCUSED.load(Ordering::Relaxed) {
                        let _ = proxy_for_ipc.send_event(UserEvent::Badge(true));
                    }
                }
            })
            .with_on_page_load_handler(move |event, url| {
                if !matches!(event, PageLoadEvent::Finished) {
                    return;
                }
                if host_of(&url) == "chat.google.com" {
                    CHAT_LOADED.store(true, Ordering::Release);
                    let _ = proxy_for_load.send_event(UserEvent::ChatLoaded);
                }
                if !url.starts_with("http") {
                    return;
                }
                write_marker(&loaded_marker, started.elapsed());

                // Optional end-to-end self test: raise one notification inside the
                // page so the whole JS -> IPC -> tray path is exercised.
                if std::env::var_os("CHATDESK_WEB_TEST_NOTIFY").is_some() {
                    let _ = proxy_for_load.send_event(UserEvent::SelfTest);
                }
            })
            .with_initialization_script(PAGE_BRIDGE)
            .build(window)?;

        let _ = webview.zoom(self.zoom);
        install_native_notification_hook(&webview, self.proxy.clone());
        Ok(webview)
    }
}

struct Timings {
    window: Option<PathBuf>,
    badge: Option<PathBuf>,
    notify: Option<PathBuf>,
}

/// Applies a zoom step to the live webview, remembers it for engine rebuilds,
/// and persists it so the next launch starts at the same size.
fn apply_zoom(
    webview: &Option<WebView>,
    engine: &mut Engine,
    settings: &Rc<RefCell<Settings>>,
    zoom: f64,
) {
    let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
    settings.borrow_mut().zoom = zoom;
    settings.borrow().save();
    engine.zoom = zoom;
    if let Some(webview) = webview {
        let _ = webview.zoom(zoom);
    }
}

/// Writes the current window geometry to the settings file so ChatDesk opens
/// exactly where and how large you left it.
fn persist_window_state(window: &Window, settings: &Rc<RefCell<Settings>>) {
    let mut current = settings.borrow_mut();
    current.maximized = window.is_maximized();
    if let Ok(position) = window.outer_position() {
        current.win_x = position.x;
        current.win_y = position.y;
    }
    let size = window.inner_size();
    current.win_w = size.width;
    current.win_h = size.height;
    current.save();
}

/// `None` (the default) keeps the engine alive in the tray for passive messages.
fn teardown_delay() -> Option<Duration> {
    if !std::env::var_os("CHATDESK_WEB_DEEP_SLEEP").is_some_and(|v| v != "0") {
        return None;
    }
    let secs = std::env::var("CHATDESK_WEB_TEARDOWN_SECS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_TEARDOWN_SECS);
    Some(Duration::from_secs(secs))
}

#[allow(clippy::too_many_arguments)]
fn run(
    event_loop: tao::event_loop::EventLoop<UserEvent>,
    window: Window,
    mut engine: Engine,
    webview: WebView,
    tray: Rc<RefCell<Option<TrayIcon>>>,
    hwnd: HWND,
    overlay_on: Option<HICON>,
    normal_tray_icon: tray_icon::Icon,
    badge_tray_icon: tray_icon::Icon,
    started: Instant,
    settings: Rc<RefCell<Settings>>,
    menu: MenuHandles,
    start_hidden: bool,
    markers: Timings,
) {
    let mut webview = Some(webview);
    let mut window_painted = false;
    let mut badge_on = false;
    let mut last_notification: Option<(String, Instant)> = None;
    let teardown_after = teardown_delay();
    let idle_trim_after = Duration::from_secs(IDLE_TRIM_SECS);
    // When the window was closed to the tray (engine shutdown pending).
    let mut hidden_since: Option<Instant> = None;
    // When the visible window lost focus (cache trim pending).
    let mut unfocused_since: Option<Instant> = None;
    // Whether the engine is currently at MemoryUsageLevel::Low.
    let mut trimmed = false;
    // Last time the hidden engine's working sets were handed back to Windows.
    let mut last_hidden_trim: Option<Instant> = None;
    let hidden_trim_every = Duration::from_secs(HIDDEN_TRIM_SECS);
    // When chat.google.com last finished loading (for the notification grace).
    let mut chat_loaded_at: Option<Instant> = None;

    if start_hidden {
        window.set_visible(false);
        if let Some(webview) = &webview {
            let _ = webview.set_visible(false);
            let _ = webview.set_memory_usage_level(MemoryUsageLevel::Low);
            trim_engine(webview);
        }
        trimmed = true;
        hidden_since = Some(Instant::now());
        last_hidden_trim = Some(Instant::now());
    }

    event_loop.run(move |event, _, control_flow| {
        if !window_painted {
            window_painted = true;
            write_marker(&markers.window, started.elapsed());
        }

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                FOCUSED.store(false, Ordering::Relaxed);
                persist_window_state(&window, &settings);
                window.set_visible(false);
                if let Some(webview) = &webview {
                    // Hidden controller: no painting, GPU tiles released.
                    let _ = webview.set_visible(false);
                    let _ = webview.set_memory_usage_level(MemoryUsageLevel::Low);
                    trim_engine(webview);
                }
                trimmed = true;
                unfocused_since = None;
                hidden_since = Some(Instant::now());
                last_hidden_trim = Some(Instant::now());
            }
            Event::WindowEvent {
                event: WindowEvent::Focused(true),
                ..
            } => {
                FOCUSED.store(true, Ordering::Relaxed);
                unfocused_since = None;
                if trimmed {
                    trimmed = false;
                    if let Some(webview) = &webview {
                        let _ = webview.set_memory_usage_level(MemoryUsageLevel::Normal);
                    }
                }
                if badge_on {
                    badge_on = false;
                    apply_badge(false, &tray, &window, hwnd, overlay_on, &normal_tray_icon);
                }
            }
            Event::WindowEvent {
                event: WindowEvent::Focused(false),
                ..
            } => {
                FOCUSED.store(false, Ordering::Relaxed);
                if hidden_since.is_none() {
                    unfocused_since = Some(Instant::now());
                }
            }
            Event::UserEvent(UserEvent::Show) => {
                FOCUSED.store(true, Ordering::Relaxed);
                hidden_since = None;
                unfocused_since = None;
                last_hidden_trim = None;
                if webview.is_none() {
                    match engine.build(&window) {
                        Ok(rebuilt) => webview = Some(rebuilt),
                        Err(err) => fatal(&format!("Could not restart the web engine: {err}")),
                    }
                }
                if let Some(webview) = &webview {
                    let _ = webview.set_memory_usage_level(MemoryUsageLevel::Normal);
                    let _ = webview.set_visible(true);
                }
                trimmed = false;
                window.set_minimized(false);
                window.set_visible(true);
                window.set_focus();
                if badge_on {
                    badge_on = false;
                    apply_badge(false, &tray, &window, hwnd, overlay_on, &normal_tray_icon);
                }
            }
            Event::UserEvent(UserEvent::Badge(true)) => {
                if !badge_on {
                    badge_on = true;
                    apply_badge(true, &tray, &window, hwnd, overlay_on, &badge_tray_icon);
                    write_marker(&markers.badge, started.elapsed());
                }
            }
            Event::UserEvent(UserEvent::Badge(false)) => {
                if badge_on {
                    badge_on = false;
                    apply_badge(false, &tray, &window, hwnd, overlay_on, &normal_tray_icon);
                }
            }
            Event::UserEvent(UserEvent::Notify { title, body }) => {
                // Collapse duplicates raised by the page and the service worker.
                let key = format!("{title}\u{1}{body}");
                let duplicate = last_notification
                    .as_ref()
                    .is_some_and(|(last, at)| *last == key && at.elapsed() < Duration::from_secs(3));
                last_notification = Some((key, Instant::now()));

                let hidden = !FOCUSED.load(Ordering::Relaxed);
                // The self-test hook raises a balloon even while focused so the
                // tray path can be verified without touching the UI.
                let self_test = std::env::var_os("CHATDESK_WEB_TEST_NOTIFY").is_some();
                // Replays of already-pending messages right after (re)load are
                // not news; the unread badge still covers them.
                let replay = chat_loaded_at
                    .map_or(true, |at| at.elapsed() < Duration::from_secs(NOTIFY_GRACE_SECS));
                let mut shown = false;
                if self_test || (hidden && !duplicate && !replay) {
                    shown = show_tray_balloon(&title, &body);
                }
                if hidden && !badge_on {
                    badge_on = true;
                    apply_badge(true, &tray, &window, hwnd, overlay_on, &badge_tray_icon);
                    write_marker(&markers.badge, started.elapsed());
                }
                if let Some(path) = &markers.notify {
                    let _ = fs::write(
                        path,
                        format!("balloon={shown} hidden={hidden} badge={badge_on} title={title}\n"),
                    );
                }
            }
            Event::UserEvent(UserEvent::ChatLoaded) => {
                chat_loaded_at = Some(Instant::now());
            }
            Event::UserEvent(UserEvent::SelfTest) => {
                // Fires a page notification; the injected bridge turns it into an
                // IPC message, exactly like a real incoming chat message.
                if let Some(webview) = &webview {
                    let _ = webview.evaluate_script(
                        "try { new Notification('ChatDesk self-test', \
                         { body: 'Tray notifications are working.' }); } catch (e) {}",
                    );
                }
            }
            Event::UserEvent(UserEvent::OpenChat) => {
                let _ = open::that(start_url());
            }
            Event::UserEvent(UserEvent::Nav(script)) => {
                if let Some(webview) = &webview {
                    let _ = webview.evaluate_script(script);
                }
            }
            Event::UserEvent(UserEvent::ZoomIn) => {
                let next = (settings.borrow().zoom * 1.1).min(MAX_ZOOM);
                apply_zoom(&webview, &mut engine, &settings, next);
            }
            Event::UserEvent(UserEvent::ZoomOut) => {
                let next = (settings.borrow().zoom / 1.1).max(MIN_ZOOM);
                apply_zoom(&webview, &mut engine, &settings, next);
            }
            Event::UserEvent(UserEvent::ZoomReset) => {
                apply_zoom(&webview, &mut engine, &settings, 1.0);
            }
            Event::UserEvent(UserEvent::ToggleLaunchAtLogin) => {
                let mut current = settings.borrow_mut();
                current.launch_at_login = !current.launch_at_login;
                set_launch_at_login(current.launch_at_login);
                menu.launch.set_checked(current.launch_at_login);
                current.save();
            }
            Event::UserEvent(UserEvent::ToggleStartHidden) => {
                let mut current = settings.borrow_mut();
                current.start_hidden = !current.start_hidden;
                menu.hidden.set_checked(current.start_hidden);
                current.save();
            }
            Event::UserEvent(UserEvent::CheckUpdates) => {
                let _ = open::that("https://github.com/virusdeflect21/ChatDesk/releases/latest");
            }
            Event::UserEvent(UserEvent::ResetAppData) => {
                // Drop the engine, wipe the isolated profile, start clean.
                webview.take();
                set_launch_at_login(false);
                if let Ok(profile) = profile_dir() {
                    let _ = fs::remove_dir_all(profile);
                }
                Settings::default().save();
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe).spawn();
                }
                *control_flow = ControlFlow::Exit;
                return;
            }
            Event::UserEvent(UserEvent::Quit) => {
                persist_window_state(&window, &settings);
                *control_flow = ControlFlow::Exit;
                return;
            }
            _ => {}
        }

        // Timers: drop caches after a stretch of being unfocused, and shut the
        // whole engine down once it has sat in the tray long enough.
        let now = Instant::now();
        let mut wake: Option<Instant> = None;

        if let Some(since) = unfocused_since {
            let due = since + idle_trim_after;
            if now >= due {
                unfocused_since = None;
                if !trimmed {
                    trimmed = true;
                    if let Some(webview) = &webview {
                        let _ = webview.set_memory_usage_level(MemoryUsageLevel::Low);
                        trim_engine(webview);
                    }
                }
            } else {
                wake = Some(due);
            }
        }

        if let (Some(since), Some(delay)) = (hidden_since, teardown_after) {
            let due = since + delay;
            if now >= due {
                hidden_since = None;
                if webview.take().is_some() {
                    // Dropping the WebView closes its controller; with no
                    // controller left, the msedgewebview2 process tree exits.
                    trim_own_working_set();
                }
            } else {
                wake = Some(wake.map_or(due, |w| w.min(due)));
            }
        }

        // Passive mode: the page keeps running (and slowly faults pages back in
        // as messages arrive), so hand its memory back to Windows periodically.
        if let (Some(last), Some(webview)) = (last_hidden_trim, &webview) {
            let due = last + hidden_trim_every;
            if now >= due {
                trim_engine(webview);
                last_hidden_trim = Some(now);
                wake = Some(wake.map_or(now + hidden_trim_every, |w| w.min(now + hidden_trim_every)));
            } else {
                wake = Some(wake.map_or(due, |w| w.min(due)));
            }
        }

        *control_flow = match wake {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        };
    });
}

/// Hands this process's idle pages back to Windows.
fn trim_own_working_set() {
    use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    unsafe {
        let _ = SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX);
    }
}

/// Empties the working set of this webview's whole msedgewebview2 tree (the
/// browser process and its renderer/utility children) plus our own. The pages
/// go to the standby list, so the RAM is free for other apps, and anything the
/// page touches again is faulted back in on demand.
fn trim_engine(webview: &WebView) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, SetProcessWorkingSetSize, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SET_QUOTA,
    };

    trim_own_working_set();

    let mut browser_pid = 0u32;
    if unsafe { webview.webview().BrowserProcessId(&mut browser_pid) }.is_err() || browser_pid == 0 {
        return;
    }

    let mut pids = vec![browser_pid];
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                if entry.th32ParentProcessID == browser_pid {
                    pids.push(entry.th32ProcessID);
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);

        for pid in pids {
            if let Ok(process) =
                OpenProcess(PROCESS_SET_QUOTA | PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            {
                let _ = SetProcessWorkingSetSize(process, usize::MAX, usize::MAX);
                let _ = CloseHandle(process);
            }
        }
    }
}

/// Registers a display name and icon for our AppUserModelID. Without this,
/// Windows has no identity to show on ChatDesk's tray notifications and can
/// present them under the wrong app.
fn register_app_identity() {
    use windows::core::{w, HSTRING};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, REG_SZ,
    };

    let Some(root) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) else {
        return;
    };
    let icon_path = root.join(PROFILE_DIR).join("chatdesk.ico");
    if !icon_path.exists() {
        let _ = fs::write(&icon_path, ico_bytes(ICON_RGBA_32, ICON_SIZE));
    }

    let set = |key: HKEY, name: &HSTRING, value: &str| {
        let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
        unsafe {
            let _ = RegSetValueExW(key, name, None, REG_SZ, Some(bytes));
        }
    };

    unsafe {
        let mut key = HKEY::default();
        if RegCreateKeyW(
            HKEY_CURRENT_USER,
            w!("Software\\Classes\\AppUserModelId\\ChatDesk.ChatDeskWeb"),
            &mut key,
        )
        .is_ok()
        {
            set(key, &HSTRING::from("DisplayName"), "ChatDesk (Google Chat)");
            set(key, &HSTRING::from("IconUri"), &icon_path.to_string_lossy());
            let _ = RegCloseKey(key);
        }
    }
}

/// A single-image 32bpp .ico built from straight RGBA bytes.
fn ico_bytes(rgba: &[u8], size: u32) -> Vec<u8> {
    let pixels = (size * size * 4) as usize;
    let mask_row = (((size + 31) / 32) * 4) as usize;
    let image_len = 40 + pixels + mask_row * size as usize;
    let mut out = Vec::with_capacity(22 + image_len);
    // ICONDIR + one ICONDIRENTRY.
    out.extend_from_slice(&[0, 0, 1, 0, 1, 0]);
    out.extend_from_slice(&[size as u8, size as u8, 0, 0, 1, 0, 32, 0]);
    out.extend_from_slice(&(image_len as u32).to_le_bytes());
    out.extend_from_slice(&22u32.to_le_bytes());
    // BITMAPINFOHEADER (height doubled for the AND mask).
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]);
    // BGRA rows, bottom-up.
    for y in (0..size as usize).rev() {
        for x in 0..size as usize {
            let i = (y * size as usize + x) * 4;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    out.resize(22 + image_len, 0);
    out
}

/// Tray menu items whose checked state is toggled at runtime.
struct MenuHandles {
    launch: Rc<CheckMenuItem>,
    hidden: Rc<CheckMenuItem>,
}

fn build_menu(settings: &Settings) -> Result<(Menu, MenuHandles)> {
    let accel = || None::<tray_icon::menu::accelerator::Accelerator>;
    let show = MenuItem::with_id("show", "Show Google Chat", true, accel());
    let back = MenuItem::with_id("back", "Back", true, accel());
    let forward = MenuItem::with_id("fwd", "Forward", true, accel());
    let reload = MenuItem::with_id("reload", "Reload", true, accel());
    let home = MenuItem::with_id("home", "Back to Chat", true, accel());
    let zoom_in = MenuItem::with_id("zoomin", "Zoom in", true, accel());
    let zoom_out = MenuItem::with_id("zoomout", "Zoom out", true, accel());
    let zoom_reset = MenuItem::with_id("zoomreset", "Actual size", true, accel());
    let launch = Rc::new(CheckMenuItem::with_id(
        "autolaunch",
        "Start with Windows",
        true,
        settings.launch_at_login,
        accel(),
    ));
    let hidden = Rc::new(CheckMenuItem::with_id(
        "starthidden",
        "Start hidden in tray",
        true,
        settings.start_hidden,
        accel(),
    ));
    let updates = MenuItem::with_id("updates", "Check for updates", true, accel());
    let reset = MenuItem::with_id("reset", "Reset app data", true, accel());
    let browser = MenuItem::with_id("browser", "Open in default browser", true, accel());
    let quit = MenuItem::with_id("quit", "Quit ChatDesk", true, accel());
    let menu = Menu::with_items(&[
        &show,
        &PredefinedMenuItem::separator(),
        &back,
        &forward,
        &reload,
        &home,
        &PredefinedMenuItem::separator(),
        &zoom_in,
        &zoom_out,
        &zoom_reset,
        &PredefinedMenuItem::separator(),
        &*launch,
        &*hidden,
        &PredefinedMenuItem::separator(),
        &updates,
        &reset,
        &browser,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;
    Ok((menu, MenuHandles { launch, hidden }))
}

/// Wires the tray menu and click handlers to the event loop.
fn install_tray_handlers(proxy: &EventLoopProxy<UserEvent>) {
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let message = match event.id().as_ref() {
            "show" => UserEvent::Show,
            "back" => UserEvent::Nav("history.back()"),
            "fwd" => UserEvent::Nav("history.forward()"),
            "reload" => UserEvent::Nav("location.reload()"),
            "home" => UserEvent::Nav("location.assign('https://chat.google.com/')"),
            "zoomin" => UserEvent::ZoomIn,
            "zoomout" => UserEvent::ZoomOut,
            "zoomreset" => UserEvent::ZoomReset,
            "autolaunch" => UserEvent::ToggleLaunchAtLogin,
            "starthidden" => UserEvent::ToggleStartHidden,
            "updates" => UserEvent::CheckUpdates,
            "reset" => UserEvent::ResetAppData,
            "browser" => UserEvent::OpenChat,
            _ => UserEvent::Quit,
        };
        let _ = menu_proxy.send_event(message);
    }));

    let click_proxy = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            let _ = click_proxy.send_event(UserEvent::Show);
        }
    }));
}

/// Hooks WebView2's own notification event. This sees page *and* service-worker
/// notifications, so the tray stays correct even when the page does not route
/// them through `window.Notification`. Returns silently on older runtimes or
/// when the runtime does not expose the event.
fn install_native_notification_hook(webview: &WebView, proxy: EventLoopProxy<UserEvent>) {
    use windows::core::{Interface, PWSTR};

    let Ok(core) = webview.webview().cast::<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_24>()
    else {
        return;
    };

    let handler = webview2_com::NotificationReceivedEventHandler::create(Box::new(
        move |_sender: Option<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2>,
              args: Option<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2NotificationReceivedEventArgs>| {
            let Some(args) = args else {
                return Ok(());
            };
            let Ok(notification) = (unsafe { args.Notification() }) else {
                return Ok(());
            };

            let mut title = PWSTR::null();
            let mut body = PWSTR::null();
            unsafe {
                let _ = notification.Title(&mut title);
                let _ = notification.Body(&mut body);
                // ChatDesk renders this in the tray, so drop WebView2's own toast.
                let _ = args.SetHandled(true);
            }
            let title = take_wide(title);
            let body = take_wide(body);
            let _ = proxy.send_event(UserEvent::Notify { title, body });
            Ok(())
        },
    ));

    let mut token = 0i64;
    unsafe {
        let _ = core.add_NotificationReceived(&handler, &mut token);
    }
}

/// Copies and frees a COM-allocated wide string.
fn take_wide(value: windows::core::PWSTR) -> String {
    if value.is_null() {
        return String::new();
    }
    let text = unsafe { value.to_string() }.unwrap_or_default();
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(value.0 as *const c_void)) };
    text
}

/// Applies the tray icon, tooltip, window title and taskbar overlay at once.
fn apply_badge(
    on: bool,
    tray: &Rc<RefCell<Option<TrayIcon>>>,
    window: &Window,
    hwnd: HWND,
    overlay_on: Option<HICON>,
    icon: &tray_icon::Icon,
) {
    if let Some(tray_icon) = tray.borrow().as_ref() {
        let _ = tray_icon.set_icon(Some(icon.clone()));
        let _ = tray_icon.set_tooltip(Some(if on {
            format!("{APP_NAME} — new messages")
        } else {
            format!("{APP_NAME} — Google Chat")
        }));
    }
    let title = if on {
        format!("• {WINDOW_TITLE}")
    } else {
        WINDOW_TITLE.to_string()
    };
    let _ = window.set_title(&title);
    let overlay = if on {
        overlay_on.unwrap_or(HICON(std::ptr::null_mut()))
    } else {
        HICON(std::ptr::null_mut())
    };
    set_taskbar_overlay(hwnd, overlay);
}

/// Pulls a leading "(N)" unread count out of the document title.
fn parse_unread(title: &str) -> u32 {
    let bytes = title.as_bytes();
    for (i, byte) in bytes.iter().enumerate() {
        if *byte != b'(' {
            continue;
        }
        let digits: String = bytes[i + 1..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .map(|b| *b as char)
            .collect();
        if !digits.is_empty() && bytes.get(i + 1 + digits.len()) == Some(&b')') {
            return digits.parse().unwrap_or(0);
        }
    }
    0
}

/// Builds a 32bpp HICON from straight RGBA bytes.
#[cfg(windows)]
fn rgba_to_hicon(rgba: &[u8], size: u32) -> Option<HICON> {
    use std::mem::zeroed;
    use std::slice;
    use windows::Win32::Graphics::Gdi::{
        CreateBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, BI_BITFIELDS,
        BITMAPINFO, BITMAPV5HEADER, DIB_RGB_COLORS, HGDIOBJ,
    };
    use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO};

    unsafe {
        let hdc = CreateCompatibleDC(None);
        if hdc.0.is_null() {
            return None;
        }

        let mut bmi: BITMAPV5HEADER = zeroed();
        bmi.bV5Size = std::mem::size_of::<BITMAPV5HEADER>() as u32;
        bmi.bV5Width = size as i32;
        bmi.bV5Height = -(size as i32);
        bmi.bV5Planes = 1;
        bmi.bV5BitCount = 32;
        bmi.bV5Compression = BI_BITFIELDS;
        bmi.bV5RedMask = 0x00FF_0000;
        bmi.bV5GreenMask = 0x0000_FF00;
        bmi.bV5BlueMask = 0x0000_00FF;
        bmi.bV5AlphaMask = 0xFF00_0000;

        let mut bits: *mut c_void = std::ptr::null_mut();
        let hbm_color = match CreateDIBSection(
            Some(hdc),
            &bmi as *const _ as *const BITMAPINFO,
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        ) {
            Ok(bitmap) => bitmap,
            Err(_) => {
                let _ = DeleteDC(hdc);
                return None;
            }
        };

        let dst = slice::from_raw_parts_mut(bits as *mut u8, (size * size * 4) as usize);
        for i in 0..(size * size) as usize {
            let source = i * 4;
            dst[source] = rgba[source + 2];
            dst[source + 1] = rgba[source + 1];
            dst[source + 2] = rgba[source];
            dst[source + 3] = rgba[source + 3];
        }

        let mask_stride = (((size + 31) / 32) * 4) as usize;
        let mask_bytes = vec![0u8; mask_stride * size as usize];
        let hbm_mask = CreateBitmap(
            size as i32,
            size as i32,
            1,
            1,
            Some(mask_bytes.as_ptr() as *const c_void),
        );

        let info = ICONINFO {
            fIcon: windows::core::BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };

        let hicon = CreateIconIndirect(&info).ok();

        let _ = DeleteObject(HGDIOBJ(hbm_color.0));
        let _ = DeleteObject(HGDIOBJ(hbm_mask.0));
        let _ = DeleteDC(hdc);

        hicon
    }
}

/// Puts a small badge on the window's taskbar button, or clears it for a null icon.
fn set_taskbar_overlay(hwnd: HWND, icon: HICON) {
    use windows::core::PCWSTR;
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{ITaskbarList3, TaskbarList};

    unsafe {
        let Ok(taskbar) =
            CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER)
        else {
            return;
        };
        let _ = taskbar.HrInit();
        let description: Vec<u16> = "New messages\0".encode_utf16().collect();
        let _ = taskbar.SetOverlayIcon(hwnd, icon, PCWSTR(description.as_ptr()));
    }
}

/// Finds the hidden window the tray-icon crate created for us (class `tray_icon_app`).
fn own_tray_hwnd() -> Option<HWND> {
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{EnumThreadWindows, GetClassNameW};

    unsafe extern "system" fn callback(hwnd: HWND, lparam: windows::Win32::Foundation::LPARAM) -> windows::core::BOOL {
        let found = &mut *(lparam.0 as *mut Option<HWND>);
        let mut buf = [0u16; 64];
        let len = GetClassNameW(hwnd, &mut buf);
        if len > 0 && String::from_utf16_lossy(&buf[..len as usize]) == "tray_icon_app" {
            *found = Some(hwnd);
            return windows::core::BOOL(0);
        }
        windows::core::BOOL(1)
    }

    let mut found: Option<HWND> = None;
    unsafe {
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(callback),
            windows::Win32::Foundation::LPARAM(&mut found as *mut _ as isize),
        );
    }
    found
}

fn copy_wide(dst: &mut [u16], text: &str) {
    let mut i = 0;
    for unit in text.encode_utf16() {
        if i + 1 >= dst.len() {
            break;
        }
        dst[i] = unit;
        i += 1;
    }
    dst[i] = 0;
}

/// Raises a genuine notification-area balloon on our tray icon.
fn show_tray_balloon(title: &str, body: &str) -> bool {
    use std::mem::size_of;
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_INFO, NIIF_INFO, NIM_MODIFY, NOTIFYICONDATAW,
    };

    let hwnd = find_tray_hwnd();
    let Some(hwnd) = hwnd else {
        return false;
    };

    let title = if title.trim().is_empty() { "Google Chat" } else { title };
    let body = if body.trim().is_empty() { "New message" } else { body };

    unsafe {
        let mut data = NOTIFYICONDATAW::default();
        data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = tray_icon_id(hwnd);
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = NIIF_INFO;
        copy_wide(&mut data.szInfoTitle, title);
        copy_wide(&mut data.szInfo, body);
        data.Anonymous.uTimeout = 6000;
        Shell_NotifyIconW(NIM_MODIFY, &data).as_bool()
    }
}

/// The id tray-icon registered our icon under. It is not documented, so ask the
/// shell which (hwnd, id) pair it actually knows about, once.
fn tray_icon_id(hwnd: HWND) -> u32 {
    static TRAY_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    let known = TRAY_ID.load(Ordering::Relaxed);
    if known != 0 {
        return known;
    }

    use std::mem::size_of;
    use windows::Win32::UI::Shell::{Shell_NotifyIconGetRect, NOTIFYICONIDENTIFIER};

    unsafe {
        for id in 0u32..=8 {
            let identifier = NOTIFYICONIDENTIFIER {
                cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
                hWnd: hwnd,
                uID: id,
                guidItem: Default::default(),
            };
            if Shell_NotifyIconGetRect(&identifier).is_ok() {
                TRAY_ID.store(id, Ordering::Relaxed);
                return id;
            }
        }
    }
    1
}

/// Locates the tray icon's hidden window, preferring our own thread.
fn find_tray_hwnd() -> Option<HWND> {
    if let Some(hwnd) = own_tray_hwnd() {
        return Some(hwnd);
    }
    use windows::core::w;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;

    unsafe {
        FindWindowW(w!("tray_icon_app"), None)
            .ok()
            .filter(|hwnd| !hwnd.0.is_null())
    }
}

/// The page to load. Overridable for testing via `CHATDESK_WEB_URL`.
fn start_url() -> String {
    std::env::var("CHATDESK_WEB_URL").unwrap_or_else(|_| START_URL.to_string())
}

fn set_app_user_model_id() {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    const AUMID: &str = "ChatDesk.ChatDeskWeb";
    let mut wide: Vec<u16> = AUMID.encode_utf16().collect();
    wide.push(0);
    unsafe {
        let _ = SetCurrentProcessExplicitAppUserModelID(PCWSTR(wide.as_ptr()));
    }
}

fn marker_path(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable).map(PathBuf::from)
}

fn write_marker(path: &Option<PathBuf>, elapsed: Duration) {
    let Some(path) = path else {
        return;
    };
    static START_UNIX_MS: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
    let unix_ms = START_UNIX_MS.get_or_init(|| {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    });
    let _ = fs::write(
        path,
        format!(
            "{} ms after process start, unix-ms {}\n",
            elapsed.as_millis(),
            unix_ms + elapsed.as_millis()
        ),
    );
}

fn profile_dir() -> Result<PathBuf> {
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .context("LOCALAPPDATA is not set")?;
    Ok(root.join(PROFILE_DIR).join("profile"))
}

fn downloads_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .map(|home| home.join("Downloads"))
        .unwrap_or_else(std::env::temp_dir)
}

fn download_name(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let raw = path.rsplit('/').next()?;
    if raw.is_empty() {
        return None;
    }
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || ".-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    if !name.contains('.') {
        name.push_str(".bin");
    }
    Some(name)
}

fn nav_allowed(url: &str) -> bool {
    matches!(
        host_of(url).as_str(),
        "" | "chat.google.com" | "accounts.google.com"
    )
}

fn host_of(url: &str) -> String {
    let Some(rest) = url.split("://").nth(1) else {
        return String::new();
    };
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn is_google(url: &str) -> bool {
    let host = host_of(url);
    host == "google.com" || host.ends_with(".google.com")
}

/// Tries to take the single-instance port. Returns `None` when another instance
/// already owns it, after asking that instance to show itself.
fn claim_single_instance() -> Option<TcpListener> {
    match TcpListener::bind(SINGLE_INSTANCE_ADDR) {
        Ok(listener) => Some(listener),
        Err(_) => {
            let _ = TcpStream::connect_timeout(
                &SINGLE_INSTANCE_ADDR.parse().expect("valid socket address"),
                Duration::from_millis(250),
            );
            None
        }
    }
}

fn app_icon() -> Icon {
    Icon::from_rgba(ICON_RGBA_32.to_vec(), ICON_SIZE, ICON_SIZE).expect("valid icon buffer")
}

fn tray_icon_image() -> Result<tray_icon::Icon> {
    tray_icon::Icon::from_rgba(ICON_RGBA_32.to_vec(), ICON_SIZE, ICON_SIZE)
        .context("invalid tray icon buffer")
}

fn fatal(message: &str) {
    use windows::{
        core::HSTRING,
        Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK},
    };
    let _ = fs::write(
        std::env::temp_dir().join("chatdesk-web-error.txt"),
        message,
    );
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(message),
            &HSTRING::from(APP_NAME),
            MB_OK | MB_ICONERROR,
        );
    }
}
