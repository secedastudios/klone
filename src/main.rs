//! Klone — archive and retrieve large media files to a Hetzner Storage Box.
//!
//! Lives in the menu bar: the tray icon shows live throughput, a click
//! opens a compact transfer popover, and the main window is a two-pane
//! browser for local libraries and the vault.

mod commands;
mod config;
mod engine;
mod keys;
mod remote;
mod update;

use std::sync::Arc;

use parking_lot::Mutex;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{ActivationPolicy, AppHandle, Emitter, Manager, PhysicalPosition, RunEvent, WindowEvent};
use tauri_plugin_notification::NotificationExt;

use commands::AppState;
use engine::{Direction, Engine, Hooks, Snapshot};

const TRAY_ID: &str = "klone";

pub fn show_main_window(app: &AppHandle) {
    let _ = app.set_activation_policy(ActivationPolicy::Regular);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
    if let Some(p) = app.get_webview_window("popover") {
        let _ = p.hide();
    }
}

fn toggle_popover(app: &AppHandle, rect: tauri::Rect) {
    let Some(p) = app.get_webview_window("popover") else { return };
    if p.is_visible().unwrap_or(false) {
        let _ = p.hide();
        return;
    }
    let scale = p.scale_factor().unwrap_or(2.0);
    let pos = rect.position.to_physical::<f64>(scale);
    let size = rect.size.to_physical::<f64>(scale);
    let width = p.outer_size().map(|s| s.width as f64).unwrap_or(380.0 * scale);
    let x = pos.x + size.width / 2.0 - width / 2.0;
    let y = pos.y + size.height + 6.0 * scale;
    let _ = p.set_position(PhysicalPosition::new(x.max(8.0), y));
    let _ = p.show();
    let _ = p.set_focus();
}

fn speed_label(bps: f64) -> String {
    let mb = bps / 1_000_000.0;
    if mb >= 100.0 { format!("{mb:.0} MB/s") } else if mb >= 1.0 { format!("{mb:.1} MB/s") } else { format!("{:.0} KB/s", bps / 1000.0) }
}

/// Text next to the menu bar icon: blank when idle, live speed when busy.
fn tray_title(s: &Snapshot) -> String {
    let st = &s.stats;
    if st.active == 0 {
        return if st.queued > 0 { "…".into() } else { String::new() };
    }
    let mut parts = Vec::new();
    if st.up_speed > 1000.0 {
        parts.push(format!("↑{}", speed_label(st.up_speed)));
    }
    if st.down_speed > 1000.0 {
        parts.push(format!("↓{}", speed_label(st.down_speed)));
    }
    if parts.is_empty() { "⋯".into() } else { parts.join(" ") }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let settings = config::load();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::save_settings,
            commands::test_connection,
            commands::key_info,
            commands::generate_key,
            commands::copy_public_key,
            commands::authorized_keys,
            commands::add_authorized_key,
            commands::remove_authorized_key,
            commands::vault_usage,
            commands::libraries,
            commands::add_library,
            commands::remove_library,
            commands::list_local,
            commands::list_remote,
            commands::archive,
            commands::retrieve,
            commands::vault_mkdir,
            commands::vault_delete,
            commands::pause,
            commands::resume,
            commands::cancel,
            commands::pause_all,
            commands::resume_all,
            commands::clear_finished,
            commands::snapshot,
            commands::reveal,
            commands::show_main,
            commands::quit,
            update::app_version,
            update::check_update,
            update::install_update,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let last = Arc::new(Mutex::new(Snapshot::default()));

            // ── tray ──
            let open = MenuItem::with_id(app, "open", "Open Klone", true, None::<&str>)?;
            let pause = MenuItem::with_id(app, "pause_all", "Pause All Transfers", true, None::<&str>)?;
            let resume = MenuItem::with_id(app, "resume_all", "Resume All Transfers", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit Klone", true, Some("CmdOrCtrl+Q"))?;
            let sep = PredefinedMenuItem::separator(app)?;
            let sep2 = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&open, &sep, &pause, &resume, &sep2, &quit])?;
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?)
                .icon_as_template(true)
                .tooltip("Klone")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| {
                    let st = app.state::<AppState>();
                    match ev.id().as_ref() {
                        "open" => show_main_window(app),
                        "pause_all" => st.engine.pause_all(),
                        "resume_all" => st.engine.resume_all(),
                        "quit" => app.exit(0),
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, ev| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, rect, .. } = ev {
                        toggle_popover(tray.app_handle(), rect);
                    }
                })
                .build(app)?;

            // ── engine ──
            let tick_handle = handle.clone();
            let tick_last = last.clone();
            let mut prev_title = String::new();
            let prev_title = Mutex::new(std::mem::take(&mut prev_title));
            let notify_handle = handle.clone();
            let engine = Engine::start(
                settings.clone(),
                Hooks {
                    tick: Box::new(move |snap| {
                        let _ = tick_handle.emit("snapshot", snap);
                        let title = tray_title(snap);
                        let mut prev = prev_title.lock();
                        if *prev != title {
                            if let Some(tray) = tick_handle.tray_by_id(TRAY_ID) {
                                let _ = tray.set_title((!title.is_empty()).then_some(title.as_str()));
                            }
                            *prev = title;
                        }
                        *tick_last.lock() = snap.clone();
                    }),
                    batch_done: Box::new(move |b| {
                        let st = notify_handle.state::<AppState>();
                        if !st.engine.settings().notifications {
                            return;
                        }
                        let verb = match b.direction {
                            Direction::Upload => "Archived",
                            Direction::Download => "Retrieved",
                        };
                        let title = if b.failed > 0 { format!("{} — {} failed", b.label, b.failed) } else { format!("{verb} {}", b.label) };
                        let body = format!("{} file{} · {}", b.files, if b.files == 1 { "" } else { "s" }, engine::fmt_bytes(b.bytes));
                        let _ = notify_handle.notification().builder().title(title).body(body).show();
                    }),
                },
            );
            app.manage(AppState { engine, last });
            update::spawn_checker(handle.clone());

            // Popover behaves like a menu: dismiss when it loses focus.
            if let Some(p) = app.get_webview_window("popover") {
                let p2 = p.clone();
                p.on_window_event(move |ev| {
                    if let WindowEvent::Focused(false) = ev {
                        let _ = p2.hide();
                    }
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the main window keeps Klone running in the menu bar.
            if window.label() == "main" {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                    let _ = window.app_handle().set_activation_policy(ActivationPolicy::Accessory);
                }
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building Klone");

    app.run(|app, event| {
        if let RunEvent::Reopen { .. } = event {
            show_main_window(app);
        }
    });
}
