//! Self-update from GitHub Releases.
//!
//! The app reads `latest.json` from the newest release of the public repo
//! (see `plugins.updater` in tauri.conf.json). Updates are signed with the
//! key in `~/.tauri/klone.key` (`make release`) and checked against the
//! public key compiled into the app, so a tampered download is refused.
//!
//! Nothing installs on its own: the UI offers the update, and installing
//! saves the queue, swaps the app bundle and restarts. Interrupted
//! transfers resume from their last committed byte, as after any quit.

use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::UpdaterExt;

use crate::commands::AppState;

const FIRST_CHECK: Duration = Duration::from_secs(20);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);

#[derive(Serialize, Clone)]
pub struct UpdateInfo {
    pub current: String,
    pub version: String,
    pub notes: Option<String>,
    pub date: Option<String>,
}

#[derive(Serialize, Clone)]
struct Progress {
    downloaded: u64,
    total: Option<u64>,
}

async fn find(app: &AppHandle) -> Result<Option<UpdateInfo>, String> {
    let update = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?;
    Ok(update.map(|u| UpdateInfo {
        current: u.current_version.clone(),
        version: u.version.clone(),
        notes: u.body.clone().filter(|b| !b.trim().is_empty()),
        date: u.date.map(|d| d.to_string()),
    }))
}

/// Check now and then every few hours; tell the UI when something's new.
pub fn spawn_checker(app: AppHandle) {
    std::thread::Builder::new()
        .name("klone-updates".into())
        .spawn(move || {
            std::thread::sleep(FIRST_CHECK);
            loop {
                match tauri::async_runtime::block_on(find(&app)) {
                    Ok(Some(info)) => {
                        log::info!("update available: {} -> {}", info.current, info.version);
                        let _ = app.emit("update-available", info);
                    // Debug builds only: install unattended, for testing the
                    // whole download → verify → replace → restart path.
                    #[cfg(debug_assertions)]
                    if std::env::var_os("KLONE_UPDATE_SELFTEST").is_some() {
                        if let Err(e) = tauri::async_runtime::block_on(install_update(app.clone())) {
                            log::error!("self-test install failed: {e}");
                        }
                    }
                    }
                    Ok(None) => {}
                    Err(e) => log::warn!("update check failed: {e}"),
                }
                std::thread::sleep(CHECK_EVERY);
            }
        })
        .ok();
}

#[tauri::command]
pub fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
pub async fn check_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    find(&app).await
}

/// Download, verify and install the latest release, then restart.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("Klone is already up to date")?;
    let mut downloaded = 0u64;
    let progress = app.clone();
    update
        .download_and_install(
            move |chunk, total| {
                downloaded += chunk as u64;
                let _ = progress.emit("update-progress", Progress { downloaded, total });
            },
            || {},
        )
        .await
        .map_err(|e| format!("Update failed: {e}"))?;
    app.state::<AppState>().engine.save_now();
    app.restart();
}
