//! Persistent settings: connection details, libraries (local roots) and
//! transfer tuning. Stored as JSON in `~/Library/Application Support/Klone`.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// What to do with the local copy after an "Offload" has been verified.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OffloadMode {
    /// Move to the macOS Trash (recoverable, space freed when emptied).
    Trash,
    /// Delete permanently (space freed immediately).
    Delete,
}

/// A local folder that mirrors into the vault under `<remote_root>/<name>`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Library {
    pub name: String,
    pub path: PathBuf,
}

/// One SFTP destination. Boxes are like drives: an archive can go to one
/// or several, and each has its own connection limit and vault folder.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct StorageBox {
    /// Stable identifier referenced by jobs; never shown.
    pub id: String,
    /// Display name, e.g. "Main" or "Offsite".
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Private key used for authentication (ssh-agent is the fallback).
    pub key_path: PathBuf,
    /// Remote directory everything is archived under.
    pub remote_root: String,
    /// Hard cap on simultaneous SSH connections to this box (Hetzner
    /// allows 10 per box; one is reserved for browsing).
    pub max_connections: usize,
}

impl Default for StorageBox {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            host: String::new(),
            port: 23,
            user: String::new(),
            key_path: crate::keys::default_key_path(),
            remote_root: "/home/klone".into(),
            max_connections: 8,
        }
    }
}

impl StorageBox {
    /// The empty box a fresh install starts with; filled in from Settings.
    fn blank() -> Self {
        Self { id: "b1".into(), name: "Vault".into(), ..Default::default() }
    }

    /// Server and username are filled in.
    pub fn is_configured(&self) -> bool {
        !self.host.trim().is_empty() && !self.user.trim().is_empty()
    }

    /// Transfer connections available (one is kept for browsing).
    pub fn transfer_cap(&self) -> usize {
        self.max_connections.saturating_sub(1).max(1)
    }

    /// Same server and credentials (i.e. an open session is still valid).
    pub fn same_endpoint(&self, o: &StorageBox) -> bool {
        self.host == o.host && self.port == o.port && self.user == o.user && self.key_path == o.key_path
    }

    /// Map a local path to its location in this box.
    ///
    /// Files inside a library mirror their relative path; anything else
    /// lands in `<remote_root>/Unsorted/<file name>`.
    pub fn remote_path_for(&self, libraries: &[Library], local: &Path) -> String {
        for lib in libraries {
            if let Ok(rel) = local.strip_prefix(&lib.path) {
                let rel = rel.to_string_lossy();
                return if rel.is_empty() {
                    format!("{}/{}", self.remote_root, lib.name)
                } else {
                    format!("{}/{}/{}", self.remote_root, lib.name, rel)
                };
            }
        }
        let name = local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        format!("{}/Unsorted/{}", self.remote_root, name)
    }

    /// Inverse of [`remote_path_for`]: where a file in this box belongs
    /// locally, if it lives under a known library.
    pub fn local_path_for(&self, libraries: &[Library], remote: &str) -> Option<PathBuf> {
        let rel = remote.strip_prefix(&self.remote_root)?;
        if !(rel.is_empty() || rel.starts_with('/')) {
            return None;
        }
        let rel = rel.trim_start_matches('/');
        let (lib_name, rest) = rel.split_once('/').unwrap_or((rel, ""));
        let lib = libraries.iter().find(|l| l.name == lib_name)?;
        Some(if rest.is_empty() { lib.path.clone() } else { lib.path.join(rest) })
    }
}

/// Pre-multi-box settings kept only so old files load. Read, never written.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct LegacyBox {
    pub(crate) host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    key_path: Option<PathBuf>,
    remote_root: Option<String>,
    max_connections: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// At least one; the first is the default destination.
    pub boxes: Vec<StorageBox>,
    /// Boxes ticked last time in the archive dialog.
    pub last_targets: Vec<String>,
    pub libraries: Vec<Library>,
    /// Files transferred at the same time, per storage box.
    pub concurrent_files: usize,
    /// Parallel SSH streams used for a single large file.
    pub streams_per_file: usize,
    /// Files above this size (MiB) are split across `streams_per_file`.
    pub segment_threshold_mb: u64,
    /// Bandwidth cap across all boxes in megabits/second. 0 = unlimited.
    pub bandwidth_limit_mbps: u64,
    /// Compare MD5 of local and remote after every transfer.
    pub verify_checksums: bool,
    pub offload_mode: OffloadMode,
    /// Keep the Mac awake while transfers are running.
    pub prevent_sleep: bool,
    pub notifications: bool,
    /// Skip dotfiles and macOS metadata when expanding folders.
    pub skip_hidden: bool,
    #[serde(flatten, skip_serializing)]
    pub(crate) legacy: LegacyBox,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            boxes: vec![StorageBox::blank()],
            last_targets: Vec::new(),
            libraries: Vec::new(),
            concurrent_files: 2,
            streams_per_file: 4,
            segment_threshold_mb: 256,
            bandwidth_limit_mbps: 0,
            verify_checksums: true,
            offload_mode: OffloadMode::Trash,
            prevent_sleep: true,
            notifications: true,
            skip_hidden: true,
            legacy: LegacyBox::default(),
        }
    }
}

impl Settings {
    /// Migrate old single-box files, give every box a unique id and name,
    /// and clamp tuning values into a range the storage boxes tolerate.
    pub fn sanitized(mut self) -> Self {
        let legacy = std::mem::take(&mut self.legacy);
        // Old files have connection fields at the top level and no "boxes"
        // (serde filled `boxes` with the default), so they win.
        if legacy.host.is_some() {
            let d = StorageBox::blank();
            self.boxes = vec![StorageBox {
                host: legacy.host.unwrap_or(d.host.clone()),
                port: legacy.port.unwrap_or(d.port),
                user: legacy.user.unwrap_or(d.user.clone()),
                key_path: legacy.key_path.unwrap_or(d.key_path.clone()),
                remote_root: legacy.remote_root.unwrap_or(d.remote_root.clone()),
                max_connections: legacy.max_connections.unwrap_or(d.max_connections),
                ..d
            }];
        }
        if self.boxes.is_empty() {
            self.boxes.push(StorageBox::blank());
        }
        let mut seen_ids = std::collections::HashSet::new();
        let mut seen_names = std::collections::HashSet::new();
        for i in 0..self.boxes.len() {
            if self.boxes[i].id.is_empty() || seen_ids.contains(&self.boxes[i].id) {
                let id = (1..).map(|n| format!("b{n}")).find(|id| !self.boxes.iter().any(|b| &b.id == id)).unwrap();
                self.boxes[i].id = id;
            }
            seen_ids.insert(self.boxes[i].id.clone());
            let b = &mut self.boxes[i];
            b.name = b.name.trim().to_string();
            if b.name.is_empty() {
                b.name = b.host.split('.').next().filter(|s| !s.is_empty()).unwrap_or("Storage box").to_string();
            }
            let base = b.name.clone();
            let mut n = 2;
            while seen_names.contains(&b.name.to_lowercase()) {
                b.name = format!("{base} {n}");
                n += 1;
            }
            seen_names.insert(b.name.to_lowercase());
            b.host = b.host.trim().to_string();
            b.user = b.user.trim().to_string();
            b.max_connections = b.max_connections.clamp(2, 10);
            b.remote_root = normalize_remote(&b.remote_root);
        }
        let min_cap = self.boxes.iter().map(|b| b.transfer_cap()).min().unwrap_or(1);
        // Every file needs at least one connection of a box's budget.
        self.concurrent_files = self.concurrent_files.clamp(1, min_cap.max(1));
        self.streams_per_file = self.streams_per_file.clamp(1, 8);
        self.segment_threshold_mb = self.segment_threshold_mb.max(16);
        self.last_targets.retain(|id| seen_ids.contains(id));
        self
    }

    pub fn box_by_id(&self, id: &str) -> Option<&StorageBox> {
        self.boxes.iter().find(|b| b.id == id)
    }

    pub fn first_box(&self) -> &StorageBox {
        &self.boxes[0]
    }
}

pub fn normalize_remote(p: &str) -> String {
    let mut out = String::from("/");
    out.push_str(&p.split('/').filter(|s| !s.is_empty() && *s != ".").collect::<Vec<_>>().join("/"));
    out
}

pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("KLONE_DATA_DIR") {
        let d = PathBuf::from(d);
        let _ = std::fs::create_dir_all(&d);
        return d;
    }
    let dir = dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("Klone");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn settings_file() -> PathBuf {
    data_dir().join("settings.json")
}

pub fn load() -> Settings {
    std::fs::read(settings_file())
        .ok()
        .and_then(|b| serde_json::from_slice::<Settings>(&b).ok())
        .unwrap_or_default()
        .sanitized()
}

pub fn save(s: &Settings) -> anyhow::Result<()> {
    write_atomic(&settings_file(), &serde_json::to_vec_pretty(s)?)
}

/// Write via temp file + rename so a crash never leaves a torn file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Box for the live (`--ignored`) tests, from the environment so no real
/// server is baked into the code:
/// `KLONE_TEST_HOST=u123.your-storagebox.de KLONE_TEST_USER=u123 [KLONE_TEST_KEY=~/.ssh/key]`
#[cfg(test)]
pub fn live_test_box() -> StorageBox {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let host = var("KLONE_TEST_HOST").expect("set KLONE_TEST_HOST (and KLONE_TEST_USER) for live tests");
    let user = var("KLONE_TEST_USER").expect("set KLONE_TEST_USER for live tests");
    StorageBox {
        id: "b1".into(),
        name: "Live test".into(),
        host,
        user,
        port: var("KLONE_TEST_PORT").and_then(|p| p.parse().ok()).unwrap_or(23),
        key_path: var("KLONE_TEST_KEY").map(PathBuf::from).unwrap_or_else(crate::keys::default_key_path),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn libs() -> Vec<Library> {
        vec![Library { name: "Projects".into(), path: "/Volumes/Media/Projects".into() }]
    }
    fn bx() -> StorageBox {
        StorageBox { remote_root: "/home/klone".into(), ..StorageBox::blank() }
    }

    #[test]
    fn maps_library_paths_both_ways() {
        let b = bx();
        let local = Path::new("/Volumes/Media/Projects/Acme/A001.mov");
        let remote = b.remote_path_for(&libs(), local);
        assert_eq!(remote, "/home/klone/Projects/Acme/A001.mov");
        assert_eq!(b.local_path_for(&libs(), &remote).unwrap(), local);
        // A sibling root that merely shares a prefix is not inside the vault.
        assert!(b.local_path_for(&libs(), "/home/klone2/Projects/x").is_none());
    }

    #[test]
    fn unknown_paths_go_to_unsorted() {
        let b = bx();
        assert_eq!(b.remote_path_for(&libs(), Path::new("/tmp/x.mov")), "/home/klone/Unsorted/x.mov");
        assert!(b.local_path_for(&libs(), "/home/klone/Unsorted/x.mov").is_none());
    }

    #[test]
    fn normalizes_remote_paths() {
        assert_eq!(normalize_remote("home//klone/"), "/home/klone");
        assert_eq!(normalize_remote("/"), "/");
    }

    #[test]
    fn migrates_single_box_settings() {
        let old = r#"{"host":"u1.your-storagebox.de","port":23,"user":"u1","key_path":"/k","remote_root":"/home/v",
            "max_connections":6,"libraries":[],"concurrent_files":2}"#;
        let s = serde_json::from_str::<Settings>(old).unwrap().sanitized();
        assert_eq!(s.boxes.len(), 1);
        let b = &s.boxes[0];
        assert_eq!((b.id.as_str(), b.host.as_str(), b.user.as_str(), b.max_connections), ("b1", "u1.your-storagebox.de", "u1", 6));
        assert_eq!(b.remote_root, "/home/v");
        // Saved form has no legacy keys.
        let out = serde_json::to_value(&s).unwrap();
        assert!(out.get("host").is_none() && out["boxes"].is_array());
        let again = serde_json::from_value::<Settings>(out).unwrap().sanitized();
        assert_eq!(again.boxes, s.boxes);
    }

    #[test]
    fn boxes_get_unique_ids_and_names() {
        let s = Settings {
            boxes: vec![
                StorageBox { name: "Main".into(), host: "a".into(), ..Default::default() },
                StorageBox { name: "main".into(), host: "b".into(), ..Default::default() },
                StorageBox { host: "u9.example.de".into(), ..Default::default() },
            ],
            ..Default::default()
        }
        .sanitized();
        let ids: std::collections::HashSet<_> = s.boxes.iter().map(|b| b.id.clone()).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(s.boxes[1].name, "main 2");
        assert_eq!(s.boxes[2].name, "u9");
    }
}
