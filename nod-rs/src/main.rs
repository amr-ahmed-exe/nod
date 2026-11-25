#![cfg_attr(
    all(not(debug_assertions), feature = "desktop"),
    windows_subsystem = "windows"
)]
#![cfg_attr(not(feature = "desktop"), allow(dead_code))]

mod api;
mod db;
mod llm;
#[allow(clippy::result_large_err)]
mod runtime;
mod server;
#[cfg(feature = "desktop")]
mod tray;
mod updater;
mod util;

use std::sync::Arc;
use std::time::Duration;

struct CliOptions {
    headless: bool,
    host: String,
    port: u16,
}

fn parse_cli_options() -> Result<CliOptions, String> {
    let mut headless = !cfg!(feature = "desktop")
        || std::env::var_os("NOD_HEADLESS").is_some();
    let mut host = std::env::var("NOD_HOST")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let mut port: u16 = std::env::var("NOD_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--headless" {
            headless = true;
        } else if arg == "--version" || arg == "-V" {
            println!("nod {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        } else if arg == "--help" || arg == "-h" {
            println!("Usage: nod [--headless] [--host <addr>] [--port <port>]");
            std::process::exit(0);
        } else if let Some(val) = arg.strip_prefix("--host=") {
            host = val.to_string();
        } else if arg == "--host" {
            host = args
                .next()
                .ok_or_else(|| "Missing value after --host".to_string())?;
        } else if let Some(val) = arg.strip_prefix("--port=") {
            port = val
                .parse()
                .map_err(|_| format!("Invalid port value: {val}"))?;
        } else if arg == "--port" {
            let val = args
                .next()
                .ok_or_else(|| "Missing value after --port".to_string())?;
            port = val
                .parse()
                .map_err(|_| format!("Invalid port value: {val}"))?;
        }
    }

    if headless && port == 0 {
        port = 3000;
    }
    #[cfg(target_os = "linux")]
    if !headless
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
    {
        headless = true;
        if port == 0 {
            port = 3000;
        }
    }

    Ok(CliOptions {
        headless,
        host,
        port,
    })
}

fn run_headless(opts: &CliOptions) -> Result<(), Box<dyn std::error::Error>> {
    let server = server::LocalServer::start_bound(&opts.host, opts.port)?;
    server.runtime.warmup();
    let updater = updater::UpdateManager::new();
    updater.start_background_checks();
    println!("Nod listening on {}", server.origin);
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let opts = parse_cli_options().map_err(std::io::Error::other)?;
    #[cfg(feature = "desktop")]
    if !opts.headless {
        return desktop_app::run_desktop(&opts);
    }
    let _ = Arc::strong_count(&Arc::new(opts.port));
    run_headless(&opts)
}

#[cfg(feature = "desktop")]
mod desktop_app {
    use super::{api, db, runtime, server, tray, updater, CliOptions};
    use crate::runtime::windows;
    use serde::{Deserialize, Serialize};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use tao::{
        dpi::LogicalSize,
        event::{Event, WindowEvent},
        event_loop::{ControlFlow, EventLoopBuilder},
        window::{ResizeDirection, WindowBuilder},
    };
    use wry::WebViewBuilder;

    #[derive(Debug, Clone, Copy)]
    enum WindowCommand {
        Minimize,
        ToggleMaximize,
        Close,
        Drag,
        DragResize(ResizeDirection),
    }

    #[derive(Debug)]
    enum BridgeEvent {
        Reply {
            id: u64,
            ok: bool,
            payload: Value,
        },
        Push {
            channel: String,
            payload: Value,
        },
        Navigate(String),
        WindowAction(WindowCommand),
        SetDarkChrome(bool),
        ShowWindow,
        TrayCheckUpdates,
        TrayRestartToUpdate,
        QuitApp,
    }

    static WINDOW_MAXIMIZED: AtomicBool = AtomicBool::new(false);

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct IpcMessage {
        id: u64,
        channel: String,
        #[serde(default)]
        args: Vec<Value>,
    }

    #[derive(Serialize)]
    struct ErrorEnvelope<'a> {
        message: &'a str,
    }

    pub fn run_desktop(opts: &CliOptions) -> Result<(), Box<dyn std::error::Error>> {
        let server = server::LocalServer::start_bound(&opts.host, opts.port)?;

        let event_loop = EventLoopBuilder::<BridgeEvent>::with_user_event().build();
        let proxy = event_loop.create_proxy();

        let notifier_proxy = proxy.clone();
        server.runtime.set_bridge_notifier(Box::new(move |channel, payload| {
            let _ = notifier_proxy.send_event(BridgeEvent::Push {
                channel: channel.to_string(),
                payload,
            });
        }));

        server.runtime.warmup();

        let updater = updater::UpdateManager::new();
        let tray_proxy = proxy.clone();
        let tray = Arc::new(tray::TrayController::start(move |action| {
            let ev = match action {
                tray::TrayAction::ShowWindow => BridgeEvent::ShowWindow,
                tray::TrayAction::CheckForUpdates => BridgeEvent::TrayCheckUpdates,
                tray::TrayAction::RestartToUpdate => BridgeEvent::TrayRestartToUpdate,
                tray::TrayAction::Quit => BridgeEvent::QuitApp,
            };
            let _ = tray_proxy.send_event(ev);
        }));

        let upd_proxy = proxy.clone();
        let upd_tray = Arc::clone(&tray);
        let upd_rt = Arc::clone(&server.runtime);
        let notified_ver = Mutex::new(None::<String>);
        updater.set_listener(Box::new(move |payload| {
            let phase = payload.get("phase").and_then(Value::as_str).unwrap_or("idle");
            let ready_ver = payload.get("readyVersion").and_then(Value::as_str);
            if let Some(t) = upd_tray.as_ref() {
                if phase == "ready"
                    && let Some(ver) = ready_ver
                {
                    t.set_update_state(&format!("Restart to update ({ver})"), true);
                    let mut should_notify = false;
                    if let Ok(mut guard) = notified_ver.lock()
                        && guard.as_deref() != Some(ver)
                    {
                        *guard = Some(ver.to_string());
                        should_notify = true;
                    }
                    if should_notify
                        && upd_rt
                            .get_preferences()
                            .get("notifications")
                            .and_then(Value::as_bool)
                            .unwrap_or(true)
                    {
                        t.show_notification(
                            "Nod update ready",
                            &format!("Nod {ver} has been downloaded and will apply on restart."),
                        );
                    }
                } else if phase == "downloading" {
                    t.set_update_state("Downloading update...", false);
                } else if phase == "checking" || phase == "preparing" {
                    t.set_update_state("Checking for updates...", false);
                } else {
                    t.set_update_state("Check for updates...", false);
                }
            }
            let _ = upd_proxy.send_event(BridgeEvent::Push {
                channel: "nod:update-status".to_string(),
                payload,
            });
        }));
        updater.start_background_checks();

        let prefs = server.runtime.get_preferences();
        let initial_theme = prefs
            .get("resolvedTheme")
            .and_then(Value::as_str)
            .unwrap_or("dark");
        let initial_is_dark = initial_theme != "light";
        let initial_bg: (u8, u8, u8, u8) = match initial_theme {
            "light" => (250, 250, 250, 255),
            "amoled" => (0, 0, 0, 255),
            "catppuccin-mocha" => (30, 30, 46, 255),
            _ => (25, 25, 25, 255),
        };

        let mut window_builder = WindowBuilder::new()
            .with_title("Nod")
            .with_decorations(false)
            .with_inner_size(LogicalSize::new(1280.0, 820.0))
            .with_min_inner_size(LogicalSize::new(800.0, 600.0));
    #[cfg(target_os = "windows")]
    {
        use tao::platform::windows::WindowBuilderExtWindows;
        window_builder = window_builder.with_undecorated_shadow(true);
    }
    let window = window_builder.build(&event_loop)?;

    #[cfg(target_os = "windows")]
    {
        use tao::platform::windows::WindowExtWindows;
        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
        let hwnd = window.hwnd() as HWND;
        let dark: i32 = if initial_is_dark { 1 } else { 0 };
        let round: i32 = 2; // DWMWCP_ROUND
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                &dark as *const _ as _,
                std::mem::size_of::<i32>() as u32,
            );
            DwmSetWindowAttribute(
                hwnd,
                33u32, // DWMWA_WINDOW_CORNER_PREFERENCE
                &round as *const _ as _,
                std::mem::size_of::<i32>() as u32,
            );
        }
    }

    let initial_path = if server.runtime.is_ready() || std::env::var_os("NOD_START_ON_MAIN").is_some() {
        "/"
    } else {
        "/setup.html"
    };
    let in_setup_view = Arc::new(AtomicBool::new(initial_path == "/setup.html"));
    let initial_url = server.bootstrap_url(initial_path);

    let init_script = build_init_script(&server.origin, &server.auth_token)?;

    let ipc_runtime = Arc::clone(&server.runtime);
    let ipc_store = Arc::clone(&server.store);
    let ipc_live = Arc::clone(&server.live);
    let ipc_updater = Arc::clone(&updater);
    let ipc_owner = server.owner_id.clone();
    let ipc_setup_flag = Arc::clone(&in_setup_view);
    let ipc_proxy = proxy.clone();
    let ipc_origin = server.origin.clone();

    let nav_origin = server.origin.clone();
    let devtools = cfg!(debug_assertions) || std::env::var_os("NOD_DEVTOOLS").is_some();

    let webview = WebViewBuilder::new()
        .with_url(&initial_url)
        .with_background_color(initial_bg)
        .with_devtools(devtools)
        .with_initialization_script(&init_script)
        .with_navigation_handler(move |uri| uri.starts_with(&nav_origin))
        .with_ipc_handler(move |req| {
            let Ok(msg) = serde_json::from_str::<IpcMessage>(req.body()) else {
                return;
            };
            let runtime = Arc::clone(&ipc_runtime);
            let store = Arc::clone(&ipc_store);
            let live = Arc::clone(&ipc_live);
            let upd = Arc::clone(&ipc_updater);
            let owner = ipc_owner.clone();
            let setup_flag = Arc::clone(&ipc_setup_flag);
            let proxy = ipc_proxy.clone();
            let origin = ipc_origin.clone();

            thread::spawn(move || {
                let res = handle_ipc(
                    &msg.channel,
                    &msg.args,
                    &runtime,
                    &store,
                    &live,
                    &upd,
                    &owner,
                    &setup_flag,
                    &proxy,
                    &origin,
                );
                match res {
                    Ok(payload) => {
                        let _ = proxy.send_event(BridgeEvent::Reply {
                            id: msg.id,
                            ok: true,
                            payload,
                        });
                    }
                    Err(message) => {
                        let _ = proxy.send_event(BridgeEvent::Reply {
                            id: msg.id,
                            ok: false,
                            payload: json!(ErrorEnvelope { message: &message }),
                        });
                    }
                }
            });
        })
        .build(&window)?;

    let shutdown_runtime = Arc::clone(&server.runtime);
    let loop_tray = Arc::clone(&tray);
    let loop_updater = Arc::clone(&updater);
    let main_origin = server.origin.clone();
    let loop_setup_flag = Arc::clone(&in_setup_view);

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if loop_tray.is_some() {
                    window.set_visible(false);
                } else {
                    let _ = shutdown_runtime.stop();
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::WindowEvent {
                event: WindowEvent::Resized(_),
                ..
            } => {
                let maximized = window.is_maximized();
                let prev = WINDOW_MAXIMIZED.swap(maximized, Ordering::SeqCst);
                if prev != maximized {
                    let js = format!(
                        "window.__nodIpcEvent && window.__nodIpcEvent('nod:window-state', {{ maximized: {maximized} }});"
                    );
                    let _ = webview.evaluate_script(&js);
                }
            }
            Event::UserEvent(BridgeEvent::WindowAction(cmd)) => match cmd {
                WindowCommand::Minimize => window.set_minimized(true),
                WindowCommand::ToggleMaximize => {
                    let next = !window.is_maximized();
                    window.set_maximized(next);
                    WINDOW_MAXIMIZED.store(next, Ordering::SeqCst);
                    let js = format!(
                        "window.__nodIpcEvent && window.__nodIpcEvent('nod:window-state', {{ maximized: {next} }});"
                    );
                    let _ = webview.evaluate_script(&js);
                }
                WindowCommand::Close => {
                    if loop_tray.is_some() {
                        window.set_visible(false);
                    } else {
                        let _ = shutdown_runtime.stop();
                        *control_flow = ControlFlow::Exit;
                    }
                }
                WindowCommand::Drag => {
                    let _ = window.drag_window();
                }
                WindowCommand::DragResize(dir) => {
                    let _ = window.drag_resize_window(dir);
                }
            },
            Event::UserEvent(BridgeEvent::ShowWindow) => {
                window.set_minimized(false);
                window.set_visible(true);
                window.set_focus();
            }
            Event::UserEvent(BridgeEvent::TrayCheckUpdates) => {
                loop_updater.trigger_check(true);
            }
            Event::UserEvent(BridgeEvent::TrayRestartToUpdate) => {
                if loop_updater.restart_and_apply().is_ok() {
                    if let Some(t) = loop_tray.as_ref() {
                        t.shutdown();
                    }
                    let _ = shutdown_runtime.stop();
                    *control_flow = ControlFlow::Exit;
                } else {
                    window.set_minimized(false);
                    window.set_visible(true);
                    window.set_focus();
                }
            }
            Event::UserEvent(BridgeEvent::QuitApp) => {
                if let Some(t) = loop_tray.as_ref() {
                    t.shutdown();
                }
                let _ = shutdown_runtime.stop();
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(BridgeEvent::SetDarkChrome(is_dark)) => {
                #[cfg(target_os = "windows")]
                {
                    use tao::platform::windows::WindowExtWindows;
                    use windows_sys::Win32::Foundation::HWND;
                    use windows_sys::Win32::Graphics::Dwm::{
                        DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE,
                    };
                    let hwnd = window.hwnd() as HWND;
                    let dark_flag: i32 = if is_dark { 1 } else { 0 };
                    unsafe {
                        DwmSetWindowAttribute(
                            hwnd,
                            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
                            &dark_flag as *const _ as _,
                            std::mem::size_of::<i32>() as u32,
                        );
                    }
                }
                let _ = is_dark;
            }
            Event::UserEvent(BridgeEvent::Reply { id, ok, payload }) => {
                if let Ok(payload_json) = serde_json::to_string(&payload) {
                    let js = format!(
                        "window.__nodIpcReply && window.__nodIpcReply({id}, {ok}, {payload_json});"
                    );
                    let _ = webview.evaluate_script(&js);
                }
            }
            Event::UserEvent(BridgeEvent::Push { channel, payload }) => {
                let ready_now = payload
                    .get("engineReady")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if let (Ok(ch_json), Ok(payload_json)) = (
                    serde_json::to_string(&channel),
                    serde_json::to_string(&payload),
                ) {
                    let js = format!(
                        "window.__nodIpcEvent && window.__nodIpcEvent({ch_json}, {payload_json});"
                    );
                    let _ = webview.evaluate_script(&js);
                }
                if channel == "nod:status"
                    && ready_now
                    && loop_setup_flag.swap(false, Ordering::SeqCst)
                {
                    let _ = webview.load_url(&format!("{main_origin}/"));
                }
            }
            Event::UserEvent(BridgeEvent::Navigate(url)) if url.starts_with(&main_origin) => {
                let _ = webview.load_url(&url);
            }
            _ => {}
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn handle_ipc(
    channel: &str,
    args: &[Value],
    runtime: &Arc<runtime::RuntimeManager>,
    store: &Arc<db::Store>,
    live: &Arc<server::Live>,
    updater: &Arc<updater::UpdateManager>,
    owner: &str,
    in_setup_view: &Arc<AtomicBool>,
    proxy: &tao::event_loop::EventLoopProxy<BridgeEvent>,
    origin: &str,
) -> Result<Value, String> {
    match channel {
        "desktop:ready" => {
            let status = runtime.status_json();
            let ready_now = status
                .get("engineReady")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if ready_now && in_setup_view.swap(false, Ordering::SeqCst) {
                let _ = proxy.send_event(BridgeEvent::Navigate(format!("{origin}/")));
            }
            Ok(json!({ "ok": true }))
        }
        "desktop:status" => Ok(runtime.status_json()),
        "desktop:warmup" => {
            let rt = Arc::clone(runtime);
            rt.warmup();
            Ok(runtime.status_json())
        }
        "desktop:prepare" => {
            let retry = args
                .first()
                .and_then(|v| v.get("retry"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(runtime.prepare(retry).to_json())
        }
        "desktop:install-wsl" => Ok(runtime.install_wsl().to_json()),
        "desktop:import-runtime" => Ok(runtime.import_runtime(None).to_json()),
        "desktop:choose-computer-directory" => Ok(runtime.choose_computer_directory(None).to_json()),
        "desktop:show-startup-log" => {
            let path = runtime.startup_log_path();
            if path.is_file() {
                windows::open_text_file(&path)?;
            } else if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
                windows::open_path(parent)?;
            }
            Ok(json!({ "ok": true }))
        }
        "desktop:get-settings" => Ok(runtime.get_settings()),
        "desktop:save-settings" => {
            let patch = args.first().cloned().unwrap_or_else(|| json!({}));
            runtime.save_settings(patch)
        }
        "desktop:get-preferences" => Ok(runtime.get_preferences()),
        "desktop:save-preferences" => {
            let patch = args.first().cloned().unwrap_or_else(|| json!({}));
            let res = runtime.save_preferences(patch)?;
            let is_dark = res.get("resolvedTheme").and_then(Value::as_str) != Some("light");
            let _ = proxy.send_event(BridgeEvent::SetDarkChrome(is_dark));
            Ok(res)
        }
        "desktop:set-theme" => {
            let theme = args
                .first()
                .and_then(Value::as_str)
                .unwrap_or("system");
            let res = runtime.save_preferences(json!({ "theme": theme }))?;
            let is_dark = res.get("resolvedTheme").and_then(Value::as_str) != Some("light");
            let _ = proxy.send_event(BridgeEvent::SetDarkChrome(is_dark));
            Ok(res)
        }
        "desktop:open-external" => {
            let url = args
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| "Missing external URL.".to_string())?;
            windows::open_url(url)?;
            Ok(json!({ "ok": true }))
        }
        "desktop:open-settings" => {
            let section = args
                .first()
                .and_then(Value::as_str)
                .unwrap_or("general");
            let _ = proxy.send_event(BridgeEvent::Push {
                channel: "nod:open-settings".to_string(),
                payload: json!({ "section": section }),
            });
            Ok(json!({ "ok": true }))
        }
        "desktop:remove-all-data" => {
            runtime.stop().map_err(|err| err.message)?;
            store.clear("messages");
            store.clear("tasks");
            store.clear("jobs");
            store.clear("memories");
            store.clear("memory_settings");
            let mut session = api::session_state(store);
            if let Some(obj) = session.as_object_mut() {
                obj.insert("onboardingCompleted".to_string(), json!(false));
            }
            store.set_setting("session", &session);
            live.notify("session");
            Ok(json!({ "ok": true }))
        }
        "desktop:get-chat-cursor" => {
            let owner_id = args.first().and_then(Value::as_str).unwrap_or(owner);
            let thread_id = args.get(1).and_then(Value::as_str).unwrap_or("");
            Ok(runtime.get_chat_read_cursor(owner_id, thread_id))
        }
        "desktop:set-chat-cursor" => {
            let owner_id = args.first().and_then(Value::as_str).unwrap_or(owner);
            let thread_id = args.get(1).and_then(Value::as_str).unwrap_or("");
            let cursor = args.get(2).cloned().unwrap_or(Value::Null);
            runtime.set_chat_read_cursor(owner_id, thread_id, cursor)?;
            Ok(json!({ "ok": true }))
        }
        "desktop:feedback-context" => Ok(json!({
            "platform": "win32",
            "mode": "local",
            "status": runtime.status_json(),
        })),
        "desktop:local-setup" => {
            let settings = runtime.get_settings();
            let step = settings
                .get("onboardingPreferenceStep")
                .and_then(|v| v.as_str().or_else(|| v.as_bool().map(|b| if b { "complete" } else { "launchAtLogin" })))
                .unwrap_or("launchAtLogin");
            Ok(json!({ "preferenceStep": step }))
        }
        "desktop:complete-preferences" => {
            let _ = runtime.save_settings(json!({ "onboardingPreferenceStep": "complete" }))?;
            let mut session = api::session_state(store);
            if let Some(obj) = session.as_object_mut() {
                obj.insert("onboardingCompleted".to_string(), json!(true));
            }
            store.set_setting("session", &session);
            live.notify("session");
            Ok(json!({ "preferenceStep": "complete" }))
        }
        "desktop:reconcile-setup" => {
            let settings = runtime.get_settings();
            let done = settings
                .get("onboardingPreferenceStep")
                .map(|v| v.as_str() == Some("complete") || v.as_bool() == Some(true))
                .unwrap_or(false);
            if done {
                let mut session = api::session_state(store);
                if let Some(obj) = session.as_object_mut() {
                    obj.insert("onboardingCompleted".to_string(), json!(true));
                }
                store.set_setting("session", &session);
                live.notify("session");
            }
            Ok(json!({ "ok": true }))
        }
        "desktop:get-connection" => Ok(json!({ "mode": "local", "url": Value::Null })),
        "desktop:save-connection" => Err("Remote server connections are disabled in Nod Windows native local mode.".into()),
        "desktop:window-state" => Ok(json!({ "maximized": WINDOW_MAXIMIZED.load(Ordering::SeqCst) })),
        "desktop:window-minimize" => {
            let _ = proxy.send_event(BridgeEvent::WindowAction(WindowCommand::Minimize));
            Ok(json!({ "ok": true }))
        }
        "desktop:window-toggle-maximize" => {
            let _ = proxy.send_event(BridgeEvent::WindowAction(WindowCommand::ToggleMaximize));
            Ok(json!({ "ok": true }))
        }
        "desktop:window-close" => {
            let _ = proxy.send_event(BridgeEvent::WindowAction(WindowCommand::Close));
            Ok(json!({ "ok": true }))
        }
        "desktop:window-drag" => {
            let _ = proxy.send_event(BridgeEvent::WindowAction(WindowCommand::Drag));
            Ok(json!({ "ok": true }))
        }
        "desktop:window-resize" => {
            let raw = args.first().and_then(Value::as_str).unwrap_or("");
            let dir = match raw {
                "n" => Some(ResizeDirection::North),
                "ne" => Some(ResizeDirection::NorthEast),
                "e" => Some(ResizeDirection::East),
                "se" => Some(ResizeDirection::SouthEast),
                "s" => Some(ResizeDirection::South),
                "sw" => Some(ResizeDirection::SouthWest),
                "w" => Some(ResizeDirection::West),
                "nw" => Some(ResizeDirection::NorthWest),
                _ => None,
            };
            if let Some(d) = dir {
                let _ = proxy.send_event(BridgeEvent::WindowAction(WindowCommand::DragResize(d)));
            }
            Ok(json!({ "ok": true }))
        }
        "desktop:update-status" => Ok(updater.status_json()),
        "desktop:check-updates" => {
            updater.trigger_check(true);
            Ok(updater.status_json())
        }
        "desktop:update-dismiss" => {
            let key = args
                .first()
                .and_then(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                })
                .unwrap_or_default();
            Ok(updater.dismiss(&key))
        }
        "desktop:update-retry" => {
            let id = args.first().and_then(Value::as_u64).unwrap_or(0);
            if updater.is_restart_failure(id) {
                updater.restart_and_apply()?;
                let _ = proxy.send_event(BridgeEvent::QuitApp);
                Ok(updater.status_json())
            } else {
                Ok(updater.retry(id))
            }
        }
        "desktop:update-download" => {
            windows::open_url(&updater::release_page_url())?;
            Ok(json!({ "ok": true }))
        }
        "desktop:update-restart" => {
            updater.restart_and_apply()?;
            let _ = proxy.send_event(BridgeEvent::QuitApp);
            Ok(json!({ "ok": true }))
        }
        "desktop:noop" => Ok(json!({ "ok": true })),
        _ => Err(format!("Unknown desktop IPC channel: {channel}")),
    }
}

fn build_init_script(origin: &str, token: &str) -> Result<String, serde_json::Error> {
    let origin_json = serde_json::to_string(origin)?;
    let token_json = serde_json::to_string(token)?;
    Ok(format!(
        r#"(function() {{
  const EXPECTED_ORIGIN = {origin_json};
  const DESKTOP_TOKEN = {token_json};
  if (window.location.origin !== EXPECTED_ORIGIN) {{
    return;
  }}

  const originalFetch = window.fetch ? window.fetch.bind(window) : null;
  if (originalFetch) {{
    window.fetch = function(input, init) {{
      const urlStr = typeof input === 'string'
        ? input
        : (input && typeof input.url === 'string' ? input.url : '');
      const isSameOriginApi = urlStr.startsWith('/api/') || urlStr.startsWith(EXPECTED_ORIGIN + '/api/');
      if (!isSameOriginApi) {{
        return originalFetch(input, init);
      }}
      const nextInit = Object.assign({{}}, init || {{}});
      const headers = new Headers(nextInit.headers || (typeof input === 'object' && input ? input.headers : undefined) || {{}});
      if (!headers.has('x-nod-desktop-token')) {{
        headers.set('x-nod-desktop-token', DESKTOP_TOKEN);
      }}
      nextInit.headers = headers;
      nextInit.credentials = 'same-origin';
      return originalFetch(input, nextInit);
    }};
  }}

  const sendFrontendLog = function(payload) {{
    try {{
      if (!originalFetch) return;
      originalFetch('/api/log-error', {{
        method: 'POST',
        credentials: 'same-origin',
        headers: {{
          'Content-Type': 'application/json',
          'x-nod-desktop-token': DESKTOP_TOKEN
        }},
        body: JSON.stringify(payload)
      }}).catch(function() {{}});
    }} catch (_) {{}}
  }};

  window.addEventListener('error', function(event) {{
    sendFrontendLog({{
      message: event.message,
      filename: event.filename,
      lineno: event.lineno,
      colno: event.colno,
      stack: event.error ? event.error.stack : ''
    }});
  }});
  window.addEventListener('unhandledrejection', function(event) {{
    sendFrontendLog({{
      reason: String(event.reason),
      stack: event.reason && event.reason.stack ? event.reason.stack : ''
    }});
  }});

  const origConsoleError = console.error ? console.error.bind(console) : null;
  if (origConsoleError) {{
    let inConsoleError = false;
    console.error = function() {{
      try {{
        origConsoleError.apply(console, arguments);
        if (!inConsoleError) {{
          inConsoleError = true;
          try {{
            const parts = Array.prototype.map.call(arguments, function(arg) {{
              if (arg instanceof Error) return arg.stack || arg.message || String(arg);
              if (arg && typeof arg === 'object') {{
                try {{ return JSON.stringify(arg); }} catch (_) {{ return String(arg); }}
              }}
              return String(arg);
            }});
            sendFrontendLog({{ consoleError: parts.join(' ') }});
          }} finally {{
            inConsoleError = false;
          }}
        }}
      }} catch (_) {{}}
    }};
  }}

  let nextId = 1;
  const pending = new Map();
  const listeners = {{
    'nod:status': new Set(),
    'nod:update-status': new Set(),
    'nod:preferences': new Set(),
    'nod:open-settings': new Set(),
    'nod:navigate': new Set(),
    'nod:focus-message': new Set(),
    'nod:startup-feedback': new Set(),
    'nod:window-state': new Set()
  }};

  window.__nodIpcReply = function(id, ok, payload) {{
    const entry = pending.get(id);
    if (!entry) return;
    pending.delete(id);
    if (ok) {{
      entry.resolve(payload);
    }} else {{
      const msg = payload && payload.message ? payload.message : 'Desktop bridge error';
      entry.reject(new Error(msg));
    }}
  }};

  window.__nodIpcEvent = function(channel, payload) {{
    const set = listeners[channel];
    if (!set) return;
    set.forEach(function(fn) {{
      try {{ fn(payload); }} catch (_) {{}}
    }});
  }};

  const invoke = function(channel, args) {{
    return new Promise(function(resolve, reject) {{
      if (!window.ipc || typeof window.ipc.postMessage !== 'function') {{
        reject(new Error('Desktop IPC unavailable'));
        return;
      }}
      const id = nextId++;
      pending.set(id, {{ resolve: resolve, reject: reject }});
      try {{
        window.ipc.postMessage(JSON.stringify({{ id: id, channel: channel, args: args || [] }}));
      }} catch (err) {{
        pending.delete(id);
        reject(err);
      }}
    }});
  }};

  const subscribe = function(channel, listener) {{
    if (typeof listener !== 'function') return function() {{}};
    const set = listeners[channel] || (listeners[channel] = new Set());
    set.add(listener);
    return function() {{ set.delete(listener); }};
  }};

  const localRuntime = Object.freeze({{
    status: function() {{ return invoke('desktop:status', []); }},
    subscribeStatus: function(fn) {{ return subscribe('nod:status', fn); }},
    prepare: function(options) {{ return invoke('desktop:prepare', [options || {{}}]); }},
    setup: function() {{ return invoke('desktop:local-setup', []); }},
    completePreferences: function() {{ return invoke('desktop:complete-preferences', []); }},
    reconcileSetup: function() {{ return invoke('desktop:reconcile-setup', []); }}
  }});

  const bridge = Object.freeze({{
    protocolVersion: 1,
    mode: 'local',
    remote: false,
    localRuntime: localRuntime,
    ready: function() {{ return invoke('desktop:ready', []); }},
    status: function() {{ return invoke('desktop:status', []); }},
    updateStatus: function() {{ return invoke('desktop:update-status', []); }},
    checkForUpdates: function() {{ return invoke('desktop:check-updates', []); }},
    dismissUpdate: function(versionOrId) {{ return invoke('desktop:update-dismiss', [versionOrId]); }},
    retryUpdate: function(id) {{ return invoke('desktop:update-retry', [id]); }},
    downloadUpdate: function() {{ return invoke('desktop:update-download', []); }},
    restartUpdate: function() {{ return invoke('desktop:update-restart', []); }},
    prepare: function(options) {{ return invoke('desktop:prepare', [options || {{}}]); }},
    warmup: function() {{ return invoke('desktop:warmup', []); }},
    installWsl: function() {{ return invoke('desktop:install-wsl', []); }},
    importRuntime: function() {{ return invoke('desktop:import-runtime', []); }},
    chooseComputerDirectory: function() {{ return invoke('desktop:choose-computer-directory', []); }},
    showStartupLog: function() {{ return invoke('desktop:show-startup-log', []); }},
    getSettings: function() {{ return invoke('desktop:get-settings', []); }},
    saveSettings: function(input) {{ return invoke('desktop:save-settings', [input || {{}}]); }},
    getPreferences: function() {{ return invoke('desktop:get-preferences', []); }},
    savePreferences: function(input) {{ return invoke('desktop:save-preferences', [input || {{}}]); }},
    setTheme: function(theme) {{ return invoke('desktop:set-theme', [theme]); }},
    openExternal: function(url) {{ return invoke('desktop:open-external', [url]); }},
    openSettings: function(section) {{ return invoke('desktop:open-settings', [section || 'general']); }},
    removeAllData: function() {{ return invoke('desktop:remove-all-data', []); }},
    getChatReadCursor: function(ownerId, threadId) {{ return invoke('desktop:get-chat-cursor', [ownerId, threadId]); }},
    setChatReadCursor: function(ownerId, threadId, cursor) {{ return invoke('desktop:set-chat-cursor', [ownerId, threadId, cursor]); }},
    getFeedbackContext: function() {{ return invoke('desktop:feedback-context', []); }},
    getConnection: function() {{ return invoke('desktop:get-connection', []); }},
    saveConnection: function(input) {{ return invoke('desktop:save-connection', [input || {{}}]); }},
    openConnectionSettings: function() {{ return invoke('desktop:noop', []); }},
    closeConnectionSettings: function() {{ return invoke('desktop:noop', []); }},
    openStartupFeedback: function() {{ return invoke('desktop:noop', []); }},
    closeStartupFeedback: function() {{ return invoke('desktop:noop', []); }},
    startupFeedback: function(input) {{ return invoke('desktop:noop', [input || {{}}]); }},
    windowControlsAvailable: true,
    minimizeWindow: function() {{ return invoke('desktop:window-minimize', []); }},
    toggleMaximizeWindow: function() {{ return invoke('desktop:window-toggle-maximize', []); }},
    closeWindow: function() {{ return invoke('desktop:window-close', []); }},
    dragWindow: function() {{ return invoke('desktop:window-drag', []); }},
    dragResizeWindow: function(direction) {{ return invoke('desktop:window-resize', [direction || '']); }},
    getWindowState: function() {{ return invoke('desktop:window-state', []); }},
    onWindowState: function(fn) {{ return subscribe('nod:window-state', fn); }},
    onStatus: function(fn) {{ return subscribe('nod:status', fn); }},
    onUpdateStatus: function(fn) {{ return subscribe('nod:update-status', fn); }},
    onPreferences: function(fn) {{ return subscribe('nod:preferences', fn); }},
    onOpenSettings: function(fn) {{ return subscribe('nod:open-settings', fn); }},
    onNavigate: function(fn) {{ return subscribe('nod:navigate', fn); }},
    onFocusMessage: function(fn) {{ return subscribe('nod:focus-message', fn); }},
    onStartupFeedback: function(fn) {{ return subscribe('nod:startup-feedback', fn); }}
  }});

  if (!window.nodDesktop) {{
    Object.defineProperty(window, 'nodDesktop', {{
      value: bridge,
      configurable: false,
      enumerable: true,
      writable: false
    }});
  }}
}})();
"#
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn init_script_exposes_desktop_local_contract() {
            let script = build_init_script("http://127.0.0.1:4321", "tok123").unwrap();
            assert!(script.contains("protocolVersion: 1"));
            assert!(script.contains("mode: 'local'"));
            assert!(script.contains("localRuntime"));
            assert!(script.contains("window.nodDesktop"));
            assert!(script.contains("x-nod-desktop-token"));
            assert!(script.contains("window.location.origin !== EXPECTED_ORIGIN"));
            assert!(script.contains("/api/log-error"));
            assert!(script.contains("console.error = function()"));
            assert!(script.contains("updateStatus"));
            assert!(script.contains("onUpdateStatus"));
            assert!(script.contains("restartUpdate"));
        }
    }
}
