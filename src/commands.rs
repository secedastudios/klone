//! Tauri commands exposed to the UI. Anything touching disk or network
//! runs on the blocking pool so the webview never stutters.

use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::State;

use std::collections::HashMap;

use crate::config::{Library, Settings, StorageBox};
use crate::engine::{self, Engine, Snapshot};
use crate::remote::{self, Conn, RemoteEntry};

pub struct AppState {
    pub engine: Arc<Engine>,
    pub last: Arc<Mutex<Snapshot>>,
}

type CmdResult<T> = Result<T, String>;

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> CmdResult<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?.map_err(|e| format!("{e:#}"))
}

#[derive(Serialize)]
pub struct LocalEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
    mtime: u64,
    /// Unfinished jobs for this path (one per box).
    jobs: Vec<u64>,
}

#[derive(Serialize)]
pub struct LocalListing {
    path: PathBuf,
    exists: bool,
    /// Where this folder lives in each box, by box id.
    remote_dirs: HashMap<String, String>,
    entries: Vec<LocalEntry>,
}

#[derive(Serialize)]
pub struct RemoteListing {
    path: String,
    entries: Vec<RemoteEntry>,
    /// Local folder this vault folder maps back to, if any.
    local_dir: Option<PathBuf>,
    jobs: Vec<(String, u64)>,
}

#[derive(Serialize)]
pub struct Usage {
    total: u64,
    used: u64,
}

#[derive(Serialize)]
pub struct LibraryInfo {
    name: String,
    path: PathBuf,
    online: bool,
    free: u64,
    total: u64,
}

#[tauri::command]
pub fn get_settings(st: State<'_, AppState>) -> Settings {
    st.engine.settings()
}

#[tauri::command]
pub async fn save_settings(st: State<'_, AppState>, settings: Settings) -> CmdResult<Settings> {
    let engine = st.engine.clone();
    blocking(move || {
        engine.update_settings(settings)?;
        Ok(engine.settings())
    })
    .await
}

/// Connect to a box as configured in the (possibly unsaved) form.
#[tauri::command]
pub async fn test_connection(storage_box: StorageBox) -> CmdResult<Usage> {
    blocking(move || {
        let mut one = Settings::default();
        one.boxes = vec![storage_box];
        let b = one.sanitized().boxes.remove(0);
        let c = Conn::connect(&b)?;
        let (total, used) = c.usage()?;
        Ok(Usage { total, used })
    })
    .await
}

/// Inspect an SSH key (does not need to be the saved one).
#[tauri::command]
pub async fn key_info(path: PathBuf) -> CmdResult<crate::keys::KeyInfo> {
    blocking(move || Ok(crate::keys::info(&path))).await
}

/// Create a new ed25519 key and copy its public half to the clipboard.
/// The UI puts the path into the box's form and saves.
#[tauri::command]
pub async fn generate_key() -> CmdResult<crate::keys::KeyInfo> {
    blocking(move || {
        let path = crate::keys::generate()?;
        let info = crate::keys::info(&path);
        if let Some(pk) = &info.public_key {
            crate::keys::copy_to_clipboard(pk).ok();
        }
        Ok(info)
    })
    .await
}

#[tauri::command]
pub async fn copy_public_key(path: PathBuf) -> CmdResult<String> {
    blocking(move || {
        let pk = crate::keys::info(&path)
            .public_key
            .ok_or_else(|| anyhow::anyhow!("No public key found for {}", path.display()))?;
        crate::keys::copy_to_clipboard(&pk)?;
        Ok(pk)
    })
    .await
}

#[derive(Serialize)]
pub struct AuthorizedKeys {
    /// Where the list lives on the box.
    path: String,
    keys: Vec<crate::keys::AuthorizedKey>,
    /// For add: false when the key was already there.
    changed: bool,
}

/// `(authorized_keys path, contents)` on the box.
fn read_authorized(c: &Conn) -> anyhow::Result<(String, String)> {
    let path = remote::join(&remote::join(&c.home()?, ".ssh"), "authorized_keys");
    let text = c.read_text(&path)?.unwrap_or_default();
    Ok((path, text))
}

/// Write a new key list, keeping a backup, then prove this Mac can still
/// sign in with a brand-new connection — if not, put the old list back.
fn write_authorized(c: &Conn, b: &StorageBox, path: &str, old: &str, new: &str) -> anyhow::Result<()> {
    if !old.is_empty() {
        c.write_text(&format!("{path}.klone-backup"), old)?;
    }
    c.mkdir_p(remote::parent(path))?;
    c.write_text(path, new)?;
    if let Err(e) = Conn::connect(b) {
        c.write_text(path, old).ok();
        anyhow::bail!("The change was undone because Klone couldn't sign in afterwards: {e:#}");
    }
    Ok(())
}

fn authorized_view(b: &StorageBox, path: String, text: &str, changed: bool) -> AuthorizedKeys {
    let own = crate::keys::own_blob(&b.key_path);
    AuthorizedKeys { keys: crate::keys::list_authorized(text, own.as_deref()), path, changed }
}

/// Keys allowed to sign in to this box.
#[tauri::command]
pub async fn authorized_keys(st: State<'_, AppState>, box_id: String) -> CmdResult<AuthorizedKeys> {
    let engine = st.engine.clone();
    blocking(move || {
        let b = engine.storage_box(&box_id)?;
        let (path, text) = engine.with_conn(&box_id, read_authorized)?;
        Ok(authorized_view(&b, path, &text, false))
    })
    .await
}

/// Let someone else's public key sign in to this box.
#[tauri::command]
pub async fn add_authorized_key(st: State<'_, AppState>, box_id: String, key: String) -> CmdResult<AuthorizedKeys> {
    let engine = st.engine.clone();
    blocking(move || {
        let b = engine.storage_box(&box_id)?;
        let line = crate::keys::normalize_new_key(&key)?;
        engine.with_conn(&box_id, |c| {
            let (path, old) = read_authorized(c)?;
            let Some(new) = crate::keys::add_authorized(&old, &line) else {
                return Ok(authorized_view(&b, path, &old, false));
            };
            write_authorized(c, &b, &path, &old, &new)?;
            Ok(authorized_view(&b, path, &new, true))
        })
    })
    .await
}

/// Revoke a key (by its base64 blob). Klone's own key can't be removed.
#[tauri::command]
pub async fn remove_authorized_key(st: State<'_, AppState>, box_id: String, id: String) -> CmdResult<AuthorizedKeys> {
    let engine = st.engine.clone();
    blocking(move || {
        let b = engine.storage_box(&box_id)?;
        if crate::keys::own_blob(&b.key_path).as_deref() == Some(id.as_str()) {
            anyhow::bail!("That's the key Klone on this Mac signs in with — removing it would lock Klone out");
        }
        engine.with_conn(&box_id, |c| {
            let (path, old) = read_authorized(c)?;
            let new = crate::keys::remove_authorized(&old, &id);
            if crate::keys::list_authorized(&new, None).is_empty() {
                anyhow::bail!("Refusing to remove the last key — nobody could sign in");
            }
            if new != old {
                write_authorized(c, &b, &path, &old, &new)?;
            }
            Ok(authorized_view(&b, path, &new, new != old))
        })
    })
    .await
}

#[tauri::command]
pub async fn vault_usage(st: State<'_, AppState>, box_id: String) -> CmdResult<Usage> {
    let engine = st.engine.clone();
    blocking(move || {
        let (total, used) = engine.with_conn(&box_id, |c| c.usage())?;
        Ok(Usage { total, used })
    })
    .await
}

#[tauri::command]
pub fn libraries(st: State<'_, AppState>) -> Vec<LibraryInfo> {
    st.engine
        .settings()
        .libraries
        .into_iter()
        .map(|l| {
            let online = l.path.is_dir();
            LibraryInfo {
                free: if online { engine::free_space(&l.path) } else { 0 },
                total: if online { engine::total_space(&l.path) } else { 0 },
                online,
                name: l.name,
                path: l.path,
            }
        })
        .collect()
}

#[tauri::command]
pub async fn add_library(st: State<'_, AppState>, path: PathBuf) -> CmdResult<Settings> {
    let engine = st.engine.clone();
    blocking(move || {
        let mut s = engine.settings();
        if s.libraries.iter().any(|l| l.path == path) {
            return Ok(s);
        }
        let base = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Library".into());
        let base = base.replace('/', "-");
        // Library names are vault folder names, so they must be unique.
        let name = (1..)
            .map(|n| if n == 1 { base.clone() } else { format!("{base} {n}") })
            .find(|n| !s.libraries.iter().any(|l| &l.name == n))
            .expect("unbounded");
        s.libraries.push(Library { name, path });
        engine.update_settings(s)?;
        Ok(engine.settings())
    })
    .await
}

#[tauri::command]
pub async fn remove_library(st: State<'_, AppState>, name: String) -> CmdResult<Settings> {
    let engine = st.engine.clone();
    blocking(move || {
        let mut s = engine.settings();
        s.libraries.retain(|l| l.name != name);
        engine.update_settings(s)?;
        Ok(engine.settings())
    })
    .await
}

#[tauri::command]
pub async fn list_local(st: State<'_, AppState>, path: PathBuf) -> CmdResult<LocalListing> {
    let engine = st.engine.clone();
    blocking(move || {
        let s = engine.settings();
        let (active, _) = engine.active_paths();
        let mut entries = Vec::new();
        let exists = path.is_dir();
        if exists {
            for e in std::fs::read_dir(&path)?.filter_map(|e| e.ok()) {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let Ok(meta) = std::fs::metadata(e.path()) else { continue };
                let p = e.path();
                entries.push(LocalEntry {
                    jobs: active.get(&p).cloned().unwrap_or_default(),
                    name,
                    is_dir: meta.is_dir(),
                    size: if meta.is_dir() { 0 } else { meta.len() },
                    mtime: engine::mtime_of(&meta),
                    path: p,
                });
            }
        }
        let remote_dirs = s.boxes.iter().map(|b| (b.id.clone(), b.remote_path_for(&s.libraries, &path))).collect();
        Ok(LocalListing { remote_dirs, path, exists, entries })
    })
    .await
}

#[tauri::command]
pub async fn list_remote(st: State<'_, AppState>, box_id: String, path: String) -> CmdResult<RemoteListing> {
    let engine = st.engine.clone();
    blocking(move || {
        let s = engine.settings();
        let b = s.box_by_id(&box_id).cloned().ok_or_else(|| anyhow::anyhow!("Unknown storage box"))?;
        let path = crate::config::normalize_remote(&path);
        let entries = engine
            .with_conn(&box_id, |c| c.list_dir(&path))?
            .into_iter()
            .filter(|e| !e.name.starts_with('.'))
            .collect::<Vec<_>>();
        let (_, active) = engine.active_paths();
        let jobs = entries
            .iter()
            .filter_map(|e| active.get(&(box_id.clone(), e.path.clone())).map(|id| (e.path.clone(), *id)))
            .collect();
        Ok(RemoteListing { local_dir: b.local_path_for(&s.libraries, &path), path, entries, jobs })
    })
    .await
}

/// Archive to the given boxes (remembered for next time).
#[tauri::command]
pub async fn archive(st: State<'_, AppState>, paths: Vec<PathBuf>, offload: bool, targets: Vec<String>) -> CmdResult<usize> {
    let engine = st.engine.clone();
    blocking(move || {
        let mut s = engine.settings();
        if !targets.is_empty() && s.last_targets != targets {
            s.last_targets = targets.clone();
            engine.update_settings(s)?;
        }
        engine.archive(&paths, offload, &targets)
    })
    .await
}

#[tauri::command]
pub async fn retrieve(st: State<'_, AppState>, box_id: String, paths: Vec<String>, dest: Option<PathBuf>) -> CmdResult<usize> {
    let engine = st.engine.clone();
    blocking(move || engine.retrieve(&box_id, &paths, dest)).await
}

#[tauri::command]
pub async fn vault_mkdir(st: State<'_, AppState>, box_id: String, path: String) -> CmdResult<()> {
    let engine = st.engine.clone();
    blocking(move || engine.with_conn(&box_id, |c| c.mkdir_p(&path))).await
}

/// Recursively delete from the vault. The UI double-confirms this.
#[tauri::command]
pub async fn vault_delete(st: State<'_, AppState>, box_id: String, paths: Vec<String>) -> CmdResult<()> {
    let engine = st.engine.clone();
    blocking(move || {
        let root = engine.settings().box_by_id(&box_id).map(|b| b.remote_root.clone()).ok_or_else(|| anyhow::anyhow!("Unknown storage box"))?;
        for p in &paths {
            let p = crate::config::normalize_remote(p);
            if p == root || !p.starts_with(&format!("{root}/")) {
                anyhow::bail!("refusing to delete {p}: outside the archive");
            }
            engine.with_conn(&box_id, |c| delete_tree(c, &p))?;
        }
        Ok(())
    })
    .await
}

fn delete_tree(c: &Conn, path: &str) -> anyhow::Result<()> {
    match c.stat(path)? {
        None => Ok(()),
        Some(st) if st.is_dir() => {
            for e in c.list_dir(path)? {
                delete_tree(c, &remote::join(path, &e.name))?;
            }
            c.rmdir(path)
        }
        Some(_) => c.unlink(path),
    }
}

#[tauri::command]
pub fn pause(st: State<'_, AppState>, ids: Vec<u64>) {
    st.engine.pause(&ids);
}

#[tauri::command]
pub fn resume(st: State<'_, AppState>, ids: Vec<u64>) {
    st.engine.resume(&ids);
}

#[tauri::command]
pub fn cancel(st: State<'_, AppState>, ids: Vec<u64>) {
    st.engine.cancel(&ids);
}

#[tauri::command]
pub fn pause_all(st: State<'_, AppState>) {
    st.engine.pause_all();
}

#[tauri::command]
pub fn resume_all(st: State<'_, AppState>) {
    st.engine.resume_all();
}

#[tauri::command]
pub fn clear_finished(st: State<'_, AppState>) {
    st.engine.clear_finished();
}

#[tauri::command]
pub fn snapshot(st: State<'_, AppState>) -> Snapshot {
    st.last.lock().clone()
}

#[tauri::command]
pub fn reveal(path: PathBuf) -> CmdResult<()> {
    let target = if path.exists() { path } else { path.parent().map(PathBuf::from).unwrap_or(path) };
    std::process::Command::new("/usr/bin/open").arg("-R").arg(target).spawn().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn show_main(app: tauri::AppHandle) {
    crate::show_main_window(&app);
}

#[tauri::command]
pub fn quit(app: tauri::AppHandle) {
    app.exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live: add a throwaway key, sign in with it, remove it, and check the
    /// box's authorized_keys is byte-identical to before.
    /// `cargo test authorized_keys_roundtrip -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn authorized_keys_roundtrip() {
        let b = crate::config::live_test_box();
        let c = Conn::connect(&b).unwrap();
        let (path, before) = read_authorized(&c).unwrap();
        eprintln!("{path}: {} keys", crate::keys::list_authorized(&before, None).len());

        let dir = std::env::temp_dir().join(format!("klone-authz-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("k");
        assert!(std::process::Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "klone-test-throwaway", "-f"]).arg(&key)
            .status().unwrap().success());
        let line = crate::keys::normalize_new_key(&std::fs::read_to_string(dir.join("k.pub")).unwrap()).unwrap();
        let blob = crate::keys::parse_key_line(&line).unwrap().1;

        let added = crate::keys::add_authorized(&before, &line).unwrap();
        write_authorized(&c, &b, &path, &before, &added).unwrap();
        let mut other = b.clone();
        other.key_path = key.clone();
        let signed_in = Conn::connect(&other).map(|_| ());

        let (_, now) = read_authorized(&c).unwrap();
        let removed = crate::keys::remove_authorized(&now, &blob);
        write_authorized(&c, &b, &path, &now, &removed).unwrap();
        let (_, after) = read_authorized(&c).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        signed_in.expect("throwaway key should sign in once added");
        assert_eq!(after, before, "authorized_keys must be restored exactly");
        eprintln!("ok: added, signed in with it, removed; file identical");
    }
}
