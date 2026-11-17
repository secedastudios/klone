//! The transfer engine: a persistent job queue, worker threads, segmented
//! parallel streams, resumable progress and end-to-end verification.
//!
//! Reliability model
//! - Every job records per-segment *committed* byte counts, persisted to
//!   `jobs.json` every few seconds. Committed means durably on the other
//!   side: acked by the SFTP server (uploads) or `fsync`ed (downloads).
//! - Data lands in a hidden `.name.klone-part` file and is only renamed to
//!   its real name after an optional MD5 comparison, so a half-written
//!   file never masquerades as a complete one.
//! - On resume, the tail of each partial segment is compared byte-for-byte
//!   against the source before continuing; any mismatch restarts that
//!   segment instead of silently corrupting the file.
//! - Network errors retry forever-ish with capped exponential backoff.
//! - Connection count is bounded by a per-box budget because each storage
//!   box rejects more than 10 concurrent connections.
//! - With several storage boxes, one job exists per (file, box). "Free
//!   space" only removes the local copy once every chosen box holds a
//!   verified copy.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use md5::{Digest, Md5};
use parking_lot::{Condvar, Mutex, RwLock};
use serde::{Deserialize, Serialize};

use crate::config::{self, OffloadMode, Settings, StorageBox};
use crate::remote::{self, Conn};

const CHUNK: usize = 4 << 20;
const MIN_SEGMENT: u64 = 64 << 20;
const TAIL_CHECK: u64 = 256 << 10;
const MAX_ATTEMPTS: u32 = 40;
const SYNC_EVERY_BYTES: u64 = 256 << 20;
const SYNC_EVERY: Duration = Duration::from_secs(5);
const PART_SUFFIX: &str = ".klone-part";
const HISTORY_LEN: usize = 120;
const TICK: Duration = Duration::from_millis(500);

// ───────────────────────────── data model ─────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Upload,
    Download,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Verifying,
    Paused,
    Retrying,
    /// The source or destination drive is unplugged; resumes by itself
    /// once it is mounted again.
    Waiting,
    Done,
    Failed,
    Canceled,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(self, JobState::Done | JobState::Failed | JobState::Canceled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    /// Committed bytes from `start`.
    pub done: u64,
}

impl Segment {
    #[cfg(test)]
    fn len(&self) -> u64 {
        self.end - self.start
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: u64,
    /// Storage box this job talks to. Empty in pre-multi-box job files;
    /// filled with the first box on load.
    #[serde(default)]
    pub box_id: String,
    pub batch: u64,
    pub batch_label: String,
    pub direction: Direction,
    pub local: PathBuf,
    pub remote: String,
    pub size: u64,
    pub mtime: u64,
    /// Remove the local copy once the archive is verified.
    pub offload: bool,
    /// Boxes that must all hold a verified copy before the local file is
    /// removed (an archive to several boxes at once).
    #[serde(default)]
    pub offload_after: Vec<String>,
    pub state: JobState,
    pub segments: Vec<Segment>,
    pub error: Option<String>,
    pub note: Option<String>,
    pub attempts: u32,
    pub created: u64,
    pub finished: Option<u64>,
    #[serde(skip)]
    retry_at: Option<Instant>,
}

impl Job {
    fn committed(&self) -> u64 {
        self.segments.iter().map(|s| s.done).sum()
    }
}

/// Live, lock-light state for a running job, shared with its streams.
struct JobCtl {
    /// 0 = run, 1 = pause, 2 = cancel.
    signal: AtomicU8,
    verifying: AtomicBool,
    verified_bytes: AtomicU64,
    segs: Mutex<Vec<SegProgress>>,
}

#[derive(Clone, Copy)]
struct SegProgress {
    start: u64,
    end: u64,
    written: u64,
    committed: u64,
}

const SIG_RUN: u8 = 0;
const SIG_PAUSE: u8 = 1;
const SIG_CANCEL: u8 = 2;

impl JobCtl {
    fn new(segments: &[Segment]) -> Self {
        Self {
            signal: AtomicU8::new(SIG_RUN),
            verifying: AtomicBool::new(false),
            verified_bytes: AtomicU64::new(0),
            segs: Mutex::new(segments.iter().map(SegProgress::from).collect()),
        }
    }
    fn stopped(&self) -> bool {
        self.signal.load(Ordering::Relaxed) != SIG_RUN
    }
    fn committed(&self) -> u64 {
        self.segs.lock().iter().map(|s| s.committed).sum()
    }
    fn replan(&self, segments: &[Segment]) {
        *self.segs.lock() = segments.iter().map(SegProgress::from).collect();
    }
    fn reset_segment(&self, i: usize) {
        let mut segs = self.segs.lock();
        segs[i].written = 0;
        segs[i].committed = 0;
    }
    fn reset_all(&self) {
        for s in self.segs.lock().iter_mut() {
            s.written = 0;
            s.committed = 0;
        }
    }
}

impl From<&Segment> for SegProgress {
    fn from(s: &Segment) -> Self {
        Self { start: s.start, end: s.end, written: s.done, committed: s.done }
    }
}

/// Marker errors used to classify failures.
#[derive(Debug)]
struct Stopped;
impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stopped")
    }
}
impl std::error::Error for Stopped {}

/// Errors that retrying cannot fix (missing source, no disk space…).
#[derive(Debug)]
struct Fatal(String);
impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Fatal {}

fn fatal(msg: impl Into<String>) -> anyhow::Error {
    anyhow!(Fatal(msg.into()))
}

const LOCAL_MISSING: &str = "Local file unavailable";

/// `/Volumes/<name>` for paths on external drives.
pub fn volume_root(path: &Path) -> Option<PathBuf> {
    let mut it = path.components();
    let (_, vols, name) = (it.next()?, it.next()?, it.next()?);
    (vols.as_os_str() == "Volumes").then(|| Path::new("/Volumes").join(name))
}

/// False when `path` lives on an external drive that isn't mounted.
pub fn drive_online(path: &Path) -> bool {
    volume_root(path).is_none_or(|v| v.is_dir())
}

fn offline_message(path: &Path) -> String {
    let name = volume_root(path).and_then(|v| v.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default();
    format!("Drive “{name}” isn't connected — continues when it's plugged back in")
}

// ─────────────────────────── connection budget ───────────────────────────

/// Counting semaphore over SSH connections. Capacity is re-read from
/// settings so changes apply live.
struct Budget {
    in_use: Mutex<usize>,
    freed: Condvar,
}

struct Permit(Arc<Budget>);

impl Drop for Permit {
    fn drop(&mut self) {
        *self.0.in_use.lock() -= 1;
        self.0.freed.notify_all();
    }
}

impl Budget {
    fn new() -> Arc<Self> {
        Arc::new(Self { in_use: Mutex::new(0), freed: Condvar::new() })
    }
    fn try_acquire(self: &Arc<Self>, cap: usize) -> Option<Permit> {
        let mut n = self.in_use.lock();
        if *n < cap {
            *n += 1;
            Some(Permit(self.clone()))
        } else {
            None
        }
    }
}

/// A connection plus the budget slot it occupies on its box.
struct Leased {
    conn: Conn,
    box_id: String,
    _permit: Permit,
}

impl std::ops::Deref for Leased {
    type Target = Conn;
    fn deref(&self) -> &Conn {
        &self.conn
    }
}

// ───────────────────────────── rate limiting ─────────────────────────────

/// Deficit token bucket shared by every stream.
struct Limiter {
    state: Mutex<(f64, Instant)>,
}

impl Limiter {
    fn take(&self, bytes: usize, mbps: u64) {
        if mbps == 0 {
            return;
        }
        let rate = mbps as f64 * 1_000_000.0 / 8.0;
        let wait = {
            let mut st = self.state.lock();
            let now = Instant::now();
            let burst = rate * 0.25;
            st.0 = (st.0 + now.duration_since(st.1).as_secs_f64() * rate).min(burst);
            st.1 = now;
            st.0 -= bytes as f64;
            if st.0 < 0.0 { -st.0 / rate } else { 0.0 }
        };
        if wait > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(wait));
        }
    }
}

// ───────────────────────────── snapshots ─────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct SegmentView {
    pub start: u64,
    pub end: u64,
    pub done: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub id: u64,
    pub box_id: String,
    pub box_name: String,
    pub batch: u64,
    pub batch_label: String,
    pub direction: Direction,
    pub name: String,
    pub local: PathBuf,
    pub remote: String,
    pub size: u64,
    pub done: u64,
    pub state: JobState,
    pub error: Option<String>,
    pub note: Option<String>,
    pub attempts: u32,
    pub speed: f64,
    pub eta: Option<f64>,
    pub retry_in: Option<f64>,
    pub verified: u64,
    pub segments: Vec<SegmentView>,
    pub offload: bool,
    pub finished: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Default)]
pub struct Sample {
    pub up: f64,
    pub down: f64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct BoxStats {
    pub id: String,
    pub name: String,
    pub connections: usize,
    pub max_connections: usize,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Stats {
    pub up_speed: f64,
    pub down_speed: f64,
    pub active: usize,
    pub queued: usize,
    pub paused: usize,
    pub failed: usize,
    pub remaining_bytes: u64,
    pub eta: Option<f64>,
    /// Totals across all boxes.
    pub connections: usize,
    pub max_connections: usize,
    pub boxes: Vec<BoxStats>,
    /// Jobs parked until their drive is plugged back in.
    pub waiting: usize,
    /// Names of unplugged drives that jobs are waiting for.
    pub offline_drives: Vec<String>,
    /// Count of all completed jobs (the job list itself is truncated), so
    /// the UI can tell when to refresh listings.
    pub done_total: usize,
    pub session_bytes: u64,
    pub history: Vec<Sample>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Snapshot {
    pub jobs: Vec<JobView>,
    pub stats: Stats,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchSummary {
    pub label: String,
    pub direction: Direction,
    pub files: usize,
    pub bytes: u64,
    pub failed: usize,
}

pub struct Hooks {
    pub tick: Box<dyn Fn(&Snapshot) + Send + Sync>,
    pub batch_done: Box<dyn Fn(BatchSummary) + Send + Sync>,
}

// ───────────────────────────── the engine ─────────────────────────────

#[derive(Default, Serialize, Deserialize)]
struct Inner {
    jobs: Vec<Job>,
    next_id: u64,
    next_batch: u64,
    /// Verified copies per local file, used to decide when an archive to
    /// several boxes may free the local copy. Survives "Clear finished".
    #[serde(default)]
    verified: HashMap<PathBuf, Vec<VerifiedCopy>>,
    #[serde(skip)]
    dirty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VerifiedCopy {
    box_id: String,
    size: u64,
    mtime: u64,
}

type BrowseSlot = Arc<Mutex<Option<Conn>>>;

pub struct Engine {
    settings: RwLock<Settings>,
    inner: Mutex<Inner>,
    controls: Mutex<HashMap<u64, Arc<JobCtl>>>,
    wake: (Mutex<()>, Condvar),
    /// Transfer connection budget per box id.
    budgets: Mutex<HashMap<String, Arc<Budget>>>,
    /// Cached browsing connection per box id (outside the budget).
    browse: Mutex<HashMap<String, BrowseSlot>>,
    limiter: Limiter,
    hooks: Hooks,
}

/// Upper bound on files in flight across all boxes; the per-box
/// `concurrent_files` setting and connection budgets do the real limiting.
const MAX_WORKERS: usize = 24;

impl Engine {
    pub fn start(settings: Settings, hooks: Hooks) -> Arc<Self> {
        let mut inner = load_jobs();
        let first = settings.first_box().id.clone();
        for j in &mut inner.jobs {
            if j.box_id.is_empty() {
                j.box_id = first.clone();
            }
            // Anything mid-flight when we quit simply goes back in line.
            if matches!(j.state, JobState::Running | JobState::Verifying | JobState::Retrying) {
                j.state = JobState::Queued;
            }
            // Earlier versions failed jobs whose drive was unplugged; give
            // them another go (they wait if the drive is still missing).
            if j.state == JobState::Failed && j.error.as_deref().is_some_and(|e| e.starts_with(LOCAL_MISSING)) {
                j.state = JobState::Queued;
                j.error = None;
                j.attempts = 0;
                j.finished = None;
            }
        }
        let engine = Arc::new(Self {
            settings: RwLock::new(settings),
            inner: Mutex::new(inner),
            controls: Mutex::new(HashMap::new()),
            wake: (Mutex::new(()), Condvar::new()),
            budgets: Mutex::new(HashMap::new()),
            browse: Mutex::new(HashMap::new()),
            limiter: Limiter { state: Mutex::new((0.0, Instant::now())) },
            hooks,
        });
        for idx in 0..MAX_WORKERS {
            let e = engine.clone();
            std::thread::Builder::new()
                .name(format!("klone-worker-{idx}"))
                .spawn(move || e.worker())
                .expect("spawn worker");
        }
        let e = engine.clone();
        std::thread::Builder::new().name("klone-reporter".into()).spawn(move || e.reporter()).expect("spawn reporter");
        engine
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().clone()
    }

    pub fn update_settings(&self, s: Settings) -> Result<()> {
        let s = s.sanitized();
        config::save(&s)?;
        let old = std::mem::replace(&mut *self.settings.write(), s.clone());
        // Drop cached sessions whose endpoint changed or whose box is gone.
        self.browse.lock().retain(|id, _| match (old.box_by_id(id), s.box_by_id(id)) {
            (Some(a), Some(b)) => a.same_endpoint(b),
            _ => false,
        });
        // Jobs for a removed box can never run.
        {
            let mut inner = self.inner.lock();
            for j in inner.jobs.iter_mut().filter(|j| !j.state.is_terminal() && s.box_by_id(&j.box_id).is_none()) {
                if !matches!(j.state, JobState::Running | JobState::Verifying) {
                    j.state = JobState::Failed;
                    j.error = Some("Storage box was removed from Settings".into());
                    j.finished = Some(now_secs());
                }
            }
            inner.dirty = true;
        }
        for b in self.budgets.lock().values() {
            b.freed.notify_all();
        }
        self.notify();
        Ok(())
    }

    fn budget(&self, box_id: &str) -> Arc<Budget> {
        self.budgets.lock().entry(box_id.to_string()).or_insert_with(Budget::new).clone()
    }

    /// Write the queue to disk now (before a restart for an update).
    pub fn save_now(&self) {
        let bytes = {
            let mut inner = self.inner.lock();
            inner.dirty = false;
            serde_json::to_vec(&*inner)
        };
        match bytes {
            Ok(b) => {
                if let Err(e) = config::write_atomic(&jobs_file(), &b) {
                    log::error!("saving jobs: {e:#}");
                }
            }
            Err(e) => log::error!("serializing jobs: {e}"),
        }
    }

    pub fn storage_box(&self, box_id: &str) -> Result<StorageBox> {
        self.settings
            .read()
            .box_by_id(box_id)
            .cloned()
            .ok_or_else(|| fatal("Storage box was removed from Settings"))
    }

    fn notify(&self) {
        self.wake.1.notify_all();
    }

    /// Run `f` on the box's shared browsing connection (reserved outside
    /// the transfer budget), reconnecting once if the cached session died.
    pub fn with_conn<T>(&self, box_id: &str, mut f: impl FnMut(&Conn) -> Result<T>) -> Result<T> {
        let b = self.storage_box(box_id)?;
        let slot = self.browse.lock().entry(box_id.to_string()).or_default().clone();
        let mut guard = slot.lock();
        if let Some(c) = guard.as_ref() {
            match f(c) {
                Ok(v) => return Ok(v),
                Err(e) if e.downcast_ref::<ssh2::Error>().is_none() && !is_io_like(&e) => return Err(e),
                Err(_) => *guard = None,
            }
        }
        let c = Conn::connect(&b)?;
        let out = f(&c);
        *guard = Some(c);
        out
    }

    // ─────────────── enqueueing ───────────────

    /// Queue local files/folders for upload to each of `targets` (box
    /// ids; empty = the first box). With `offload`, the local copy is freed
    /// once every target holds a verified copy.
    pub fn archive(&self, paths: &[PathBuf], offload: bool, targets: &[String]) -> Result<usize> {
        let s = self.settings();
        let mut boxes: Vec<&StorageBox> = targets.iter().filter_map(|id| s.box_by_id(id)).collect();
        boxes.dedup_by_key(|b| b.id.clone());
        if boxes.is_empty() {
            if !targets.is_empty() {
                bail!("None of the chosen storage boxes exist any more");
            }
            boxes.push(s.first_box());
        }
        if let Some(b) = boxes.iter().find(|b| !b.is_configured()) {
            bail!("Set up “{}” in Settings first (server and username)", b.name);
        }
        let target_ids: Vec<String> = boxes.iter().map(|b| b.id.clone()).collect();
        let mut files = Vec::new();
        for p in paths {
            walk_local(p, s.skip_hidden, &mut files)?;
        }
        let label = batch_label(paths.iter().map(|p| p.to_string_lossy().into_owned()).collect());
        let mut inner = self.inner.lock();
        // (box, local) → index of an unfinished upload already queued.
        let active: HashMap<(String, PathBuf), usize> = inner
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, j)| j.direction == Direction::Upload && !j.state.is_terminal())
            .map(|(i, j)| ((j.box_id.clone(), j.local.clone()), i))
            .collect();
        inner.next_batch += 1;
        let batch = inner.next_batch;
        let mut added = 0;
        for (path, size, mtime) in files {
            for b in &boxes {
                if let Some(&i) = active.get(&(b.id.clone(), path.clone())) {
                    // Already on its way: just upgrade it to free space.
                    if offload {
                        let j = &mut inner.jobs[i];
                        j.offload = true;
                        for id in &target_ids {
                            if !j.offload_after.contains(id) {
                                j.offload_after.push(id.clone());
                            }
                        }
                    }
                    continue;
                }
                let remote = b.remote_path_for(&s.libraries, &path);
                let mut job = new_job(&mut inner, batch, &label, Direction::Upload, path.clone(), remote, size, mtime, offload, &s);
                job.box_id = b.id.clone();
                if offload {
                    job.offload_after = target_ids.clone();
                }
                inner.jobs.push(job);
                added += 1;
            }
        }
        inner.dirty = true;
        drop(inner);
        self.notify();
        Ok(added)
    }

    /// Queue vault files/folders for download. Without `dest` they return
    /// to their library location (or `~/Downloads/Klone`).
    pub fn retrieve(&self, box_id: &str, remote_paths: &[String], dest: Option<PathBuf>) -> Result<usize> {
        let s = self.settings();
        let b = self.storage_box(box_id)?;
        let mut files = Vec::new();
        self.with_conn(box_id, |c| {
            files.clear();
            for p in remote_paths {
                let base = remote::parent(p).to_string();
                walk_remote(c, p, &base, s.skip_hidden, &mut files)?;
            }
            Ok(())
        })?;
        let label = batch_label(remote_paths.to_vec());
        let downloads = dirs::download_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join("Klone");
        let mut inner = self.inner.lock();
        let active: HashSet<(String, String)> = inner
            .jobs
            .iter()
            .filter(|j| j.direction == Direction::Download && !j.state.is_terminal())
            .map(|j| (j.box_id.clone(), j.remote.clone()))
            .collect();
        inner.next_batch += 1;
        let batch = inner.next_batch;
        let mut added = 0;
        for (remote, rel, size, mtime) in files {
            if active.contains(&(b.id.clone(), remote.clone())) {
                continue;
            }
            let local = match &dest {
                Some(d) => d.join(&rel),
                None => b.local_path_for(&s.libraries, &remote).unwrap_or_else(|| {
                    let rel = remote.strip_prefix(&b.remote_root).unwrap_or(&remote);
                    downloads.join(rel.trim_start_matches('/'))
                }),
            };
            let local = non_clobbering(local, size);
            let mut job = new_job(&mut inner, batch, &label, Direction::Download, local, remote, size, mtime, false, &s);
            job.box_id = b.id.clone();
            inner.jobs.push(job);
            added += 1;
        }
        inner.dirty = true;
        drop(inner);
        self.notify();
        Ok(added)
    }

    // ─────────────── controls ───────────────

    pub fn pause(&self, ids: &[u64]) {
        let controls = self.controls.lock();
        let mut inner = self.inner.lock();
        for j in inner.jobs.iter_mut().filter(|j| ids.contains(&j.id)) {
            match j.state {
                JobState::Running | JobState::Verifying => {
                    if let Some(c) = controls.get(&j.id) {
                        c.signal.store(SIG_PAUSE, Ordering::Relaxed);
                    }
                }
                JobState::Queued | JobState::Retrying | JobState::Waiting => j.state = JobState::Paused,
                _ => {}
            }
        }
        inner.dirty = true;
    }

    pub fn resume(&self, ids: &[u64]) {
        let mut inner = self.inner.lock();
        for j in inner.jobs.iter_mut().filter(|j| ids.contains(&j.id)) {
            if matches!(j.state, JobState::Paused | JobState::Failed | JobState::Canceled | JobState::Retrying) {
                j.state = JobState::Queued;
                j.attempts = 0;
                j.error = None;
                j.retry_at = None;
            }
        }
        inner.dirty = true;
        drop(inner);
        self.notify();
    }

    pub fn cancel(self: &Arc<Self>, ids: &[u64]) {
        let controls = self.controls.lock();
        let mut inner = self.inner.lock();
        let mut cleanup = Vec::new();
        for j in inner.jobs.iter_mut().filter(|j| ids.contains(&j.id)) {
            match j.state {
                JobState::Running | JobState::Verifying => {
                    if let Some(c) = controls.get(&j.id) {
                        c.signal.store(SIG_CANCEL, Ordering::Relaxed);
                    }
                }
                s if !s.is_terminal() => {
                    j.state = JobState::Canceled;
                    j.finished = Some(now_secs());
                    if j.committed() > 0 {
                        cleanup.push(j.clone());
                    }
                    for seg in &mut j.segments {
                        seg.done = 0;
                    }
                }
                _ => {}
            }
        }
        inner.dirty = true;
        drop(inner);
        drop(controls);
        if !cleanup.is_empty() {
            let me = self.clone();
            std::thread::spawn(move || {
                for j in &cleanup {
                    match j.direction {
                        Direction::Upload => {
                            me.with_conn(&j.box_id, |c| c.unlink(&part_remote(&j.remote))).ok();
                        }
                        Direction::Download => remove_partial(None, j),
                    }
                }
            });
        }
    }

    pub fn pause_all(&self) {
        let ids: Vec<u64> = self.inner.lock().jobs.iter().filter(|j| !j.state.is_terminal()).map(|j| j.id).collect();
        self.pause(&ids);
    }

    pub fn resume_all(&self) {
        let ids: Vec<u64> = self
            .inner
            .lock()
            .jobs
            .iter()
            .filter(|j| matches!(j.state, JobState::Paused | JobState::Failed))
            .map(|j| j.id)
            .collect();
        self.resume(&ids);
    }

    pub fn clear_finished(&self) {
        let mut inner = self.inner.lock();
        inner.jobs.retain(|j| !matches!(j.state, JobState::Done | JobState::Canceled));
        inner.dirty = true;
    }

    /// Paths with an unfinished job, used to badge rows in the browsers:
    /// local path → job ids (one per box), and (box, remote path) → job id.
    pub fn active_paths(&self) -> (HashMap<PathBuf, Vec<u64>>, HashMap<(String, String), u64>) {
        let inner = self.inner.lock();
        let mut local: HashMap<PathBuf, Vec<u64>> = HashMap::new();
        let mut remote = HashMap::new();
        for j in inner.jobs.iter().filter(|j| !j.state.is_terminal()) {
            local.entry(j.local.clone()).or_default().push(j.id);
            remote.insert((j.box_id.clone(), j.remote.clone()), j.id);
        }
        (local, remote)
    }

    // ─────────────── scheduling ───────────────

    fn worker(self: Arc<Self>) {
        let mut conn: Option<Leased> = None;
        let mut idle_since = Instant::now();
        loop {
            if let Some(bid) = conn.as_ref().map(|c| c.box_id.clone()) {
                if let Some((job, ctl)) = self.take_next(&bid) {
                    let res = self.run_job(&mut conn, None, &job, &ctl);
                    self.finish(&job, &ctl, res, &mut conn);
                    idle_since = Instant::now();
                    continue;
                }
                // Release idle slots so streams, other workers or other
                // boxes can use them.
                if idle_since.elapsed() > Duration::from_secs(20) || self.ready_boxes().iter().any(|b| *b != bid) {
                    conn = None;
                    continue;
                }
            } else {
                let mut ran = false;
                for bid in self.ready_boxes() {
                    let Ok(b) = self.storage_box(&bid) else { continue };
                    // Never take a job without a connection slot to run it.
                    let Some(permit) = self.budget(&bid).try_acquire(b.transfer_cap()) else { continue };
                    if let Some((job, ctl)) = self.take_next(&bid) {
                        // Slot reserved; connect lazily inside the job so
                        // connection failures are attributed to the job.
                        let res = self.run_job(&mut conn, Some(permit), &job, &ctl);
                        self.finish(&job, &ctl, res, &mut conn);
                        idle_since = Instant::now();
                        ran = true;
                        break;
                    }
                }
                if ran {
                    continue;
                }
            }
            let mut g = self.wake.0.lock();
            self.wake.1.wait_for(&mut g, Duration::from_millis(750));
        }
    }

    /// Boxes with a runnable job and room for another file in flight.
    fn ready_boxes(&self) -> Vec<String> {
        let s = self.settings.read();
        let inner = self.inner.lock();
        let now = Instant::now();
        s.boxes
            .iter()
            .filter(|b| {
                let running = inner.jobs.iter().filter(|j| j.box_id == b.id && is_running(j)).count();
                running < s.concurrent_files && inner.jobs.iter().any(|j| j.box_id == b.id && runnable(j, now))
            })
            .map(|b| b.id.clone())
            .collect()
    }

    fn take_next(&self, box_id: &str) -> Option<(Job, Arc<JobCtl>)> {
        let concurrent = self.settings.read().concurrent_files;
        let mut controls = self.controls.lock();
        let mut inner = self.inner.lock();
        let now = Instant::now();
        let running = inner.jobs.iter().filter(|j| j.box_id == box_id && is_running(j)).count();
        if running >= concurrent {
            return None;
        }
        let job = inner.jobs.iter_mut().find(|j| j.box_id == box_id && runnable(j, now))?;
        job.state = JobState::Running;
        job.retry_at = None;
        let job = job.clone();
        let ctl = Arc::new(JobCtl::new(&job.segments));
        controls.insert(job.id, ctl.clone());
        inner.dirty = true;
        Some((job, ctl))
    }

    fn finish(&self, job: &Job, ctl: &JobCtl, res: Result<String>, conn: &mut Option<Leased>) {
        let signal = ctl.signal.load(Ordering::Relaxed);
        let mut cleanup = None;
        if let Err(e) = &res {
            if e.downcast_ref::<Stopped>().is_none() {
                log::warn!("{} failed: {e:#}", job.remote);
                *conn = None; // the session may be wedged; reconnect next time
            }
        }
        let summary = {
            let mut controls = self.controls.lock();
            let mut inner = self.inner.lock();
            controls.remove(&job.id);
            let segs = ctl.segs.lock().clone();
            let Some(j) = inner.jobs.iter_mut().find(|j| j.id == job.id) else { return };
            j.segments = segs.iter().map(|s| Segment { start: s.start, end: s.end, done: s.committed }).collect();
            j.size = segs.last().map(|s| s.end).unwrap_or(j.size);
            match res {
                Ok(note) => {
                    j.state = JobState::Done;
                    j.error = None;
                    j.note = (!note.is_empty()).then_some(note);
                    j.finished = Some(now_secs());
                }
                Err(e) if e.downcast_ref::<Stopped>().is_some() => {
                    if signal == SIG_CANCEL {
                        j.state = JobState::Canceled;
                        j.finished = Some(now_secs());
                        cleanup = Some(j.clone());
                        for s in &mut j.segments {
                            s.done = 0;
                        }
                    } else {
                        j.state = JobState::Paused;
                    }
                }
                Err(_) if !drive_online(&j.local) => {
                    // Drive unplugged (before or mid-transfer): not the
                    // job's fault, so no attempt is used up.
                    j.state = JobState::Waiting;
                    j.error = Some(offline_message(&j.local));
                }
                Err(e) => {
                    j.attempts += 1;
                    j.error = Some(humanize_error(&e));
                    if e.downcast_ref::<Fatal>().is_some() || j.attempts >= MAX_ATTEMPTS {
                        j.state = JobState::Failed;
                        j.finished = Some(now_secs());
                    } else {
                        j.state = JobState::Retrying;
                        let backoff = 2u64.saturating_pow(j.attempts.min(6)).min(60);
                        j.retry_at = Some(Instant::now() + Duration::from_secs(backoff));
                    }
                }
            }
            inner.dirty = true;
            let batch = job.batch;
            let all_done = inner.jobs.iter().filter(|j| j.batch == batch).all(|j| j.state.is_terminal());
            all_done.then(|| {
                let jobs: Vec<&Job> = inner.jobs.iter().filter(|j| j.batch == batch).collect();
                BatchSummary {
                    label: job.batch_label.clone(),
                    direction: job.direction,
                    files: jobs.iter().filter(|j| j.state == JobState::Done).count(),
                    bytes: jobs.iter().filter(|j| j.state == JobState::Done).map(|j| j.size).sum(),
                    failed: jobs.iter().filter(|j| j.state == JobState::Failed).count(),
                }
            })
        };
        if let Some(job) = cleanup {
            if let Some(c) = conn.as_ref() {
                remove_partial(Some(c), &job);
            }
        }
        if let Some(s) = summary.filter(|s| s.files + s.failed > 0) {
            (self.hooks.batch_done)(s);
        }
        self.notify();
    }

    // ─────────────── job execution ───────────────

    fn run_job(&self, conn: &mut Option<Leased>, permit: Option<Permit>, job: &Job, ctl: &JobCtl) -> Result<String> {
        let s = self.settings();
        let b = self.storage_box(&job.box_id)?;
        if conn.is_none() {
            let permit = permit.ok_or_else(|| anyhow!("no connection slot"))?;
            *conn = Some(Leased { conn: Conn::connect(&b)?, box_id: b.id.clone(), _permit: permit });
        }
        let c = conn.as_ref().expect("connected");
        let res = match job.direction {
            Direction::Upload => self.upload(c, job, ctl, &s, &b),
            Direction::Download => self.download(c, job, ctl, &s, &b),
        };
        if res.is_err() && ctl.signal.load(Ordering::Relaxed) == SIG_CANCEL {
            return Err(anyhow!(Stopped));
        }
        res
    }

    /// The source changed size/mtime since queueing: start over.
    fn replan(&self, job: &Job, ctl: &JobCtl, size: u64, mtime: u64, s: &Settings) {
        let segments = plan_segments(size, s);
        ctl.replan(&segments);
        let mut inner = self.inner.lock();
        if let Some(j) = inner.jobs.iter_mut().find(|j| j.id == job.id) {
            j.size = size;
            j.mtime = mtime;
            j.segments = segments;
            inner.dirty = true;
        }
    }

    fn upload(&self, c: &Conn, job: &Job, ctl: &JobCtl, s: &Settings, b: &StorageBox) -> Result<String> {
        let meta = fs::metadata(&job.local).map_err(|e| fatal(format!("{LOCAL_MISSING}: {e}")))?;
        let (size, mtime) = (meta.len(), mtime_of(&meta));
        if size != job.size || mtime != job.mtime {
            self.replan(job, ctl, size, mtime, s);
        }
        let verify = s.verify_checksums || job.offload || self.offload_requested(job.id);
        c.mkdir_p(remote::parent(&job.remote))?;

        // Already in the vault from an earlier run?
        if ctl.committed() == 0 {
            if let Some(st) = c.stat(&job.remote)? {
                if st.size == Some(size) {
                    if verify {
                        self.verify(c, job.id, ctl, &job.local, &job.remote)?;
                    }
                    let note = self.after_archive(job, s, verify, size, mtime)?;
                    return Ok(join_note(&format!("Already in {}", b.name), &note));
                }
            }
        }

        let part = part_remote(&job.remote);
        if ctl.committed() > 0 {
            match c.stat(&part)? {
                None => ctl.reset_all(),
                Some(_) => self.check_tails(ctl, |off, len| read_local(&job.local, off, len), |off, len| read_remote(c, &part, off, len))?,
            }
        }
        if size == 0 {
            c.open_for_resume(&part)?;
        }
        self.run_segments(c, job, ctl, s, b, &part)?;

        if c.stat(&part)?.and_then(|st| st.size).unwrap_or(0) > size {
            c.truncate(&part, size)?;
        }
        if verify {
            if let Err(e) = self.verify(c, job.id, ctl, &job.local, &part) {
                if e.downcast_ref::<ChecksumMismatch>().is_some() {
                    ctl.reset_all();
                }
                return Err(e);
            }
        }
        c.set_mtime(&part, mtime).ok();
        c.replace(&part, &job.remote)?;
        self.after_archive(job, s, verify, size, mtime)
    }

    fn download(&self, c: &Conn, job: &Job, ctl: &JobCtl, s: &Settings, b: &StorageBox) -> Result<String> {
        let st = c.stat(&job.remote)?.ok_or_else(|| fatal(format!("No longer in {}", b.name)))?;
        let (size, mtime) = (st.size.unwrap_or(0), st.mtime.unwrap_or(0));
        if size != job.size || mtime != job.mtime {
            self.replan(job, ctl, size, mtime, s);
        }
        if ctl.committed() == 0 {
            if let Ok(m) = fs::metadata(&job.local) {
                if m.len() == size {
                    return Ok("Already on this Mac".into());
                }
            }
        }
        let dir = job.local.parent().ok_or_else(|| fatal("Invalid destination"))?;
        fs::create_dir_all(dir).map_err(|e| fatal(format!("Cannot create {}: {e}", dir.display())))?;
        let needed = size.saturating_sub(ctl.committed()) + (512 << 20);
        let free = free_space(dir);
        if free < needed {
            bail!(Fatal(format!("Not enough disk space: needs {}, {} free", fmt_bytes(needed), fmt_bytes(free))));
        }

        let part = part_local(&job.local);
        if ctl.committed() > 0 && !part.exists() {
            ctl.reset_all();
        }
        let f = OpenOptions::new().create(true).write(true).truncate(false).open(&part)?;
        if f.metadata()?.len() != size {
            f.set_len(size)?;
        }
        drop(f);
        if ctl.committed() > 0 {
            self.check_tails(ctl, |off, len| read_remote(c, &job.remote, off, len), |off, len| read_local(&part, off, len))?;
        }
        self.run_segments(c, job, ctl, s, b, &job.remote)?;

        if s.verify_checksums {
            if let Err(e) = self.verify(c, job.id, ctl, &part, &job.remote) {
                if e.downcast_ref::<ChecksumMismatch>().is_some() {
                    ctl.reset_all();
                }
                return Err(e);
            }
        }
        filetime::set_file_mtime(&part, filetime::FileTime::from_unix_time(mtime as i64, 0)).ok();
        fs::rename(&part, &job.local)?;
        Ok(String::new())
    }

    /// Compare the last bytes of every partially-done segment on both sides;
    /// restart any segment whose tail differs.
    fn check_tails(
        &self,
        ctl: &JobCtl,
        src: impl Fn(u64, usize) -> Result<Vec<u8>>,
        dst: impl Fn(u64, usize) -> Result<Vec<u8>>,
    ) -> Result<()> {
        let segs = ctl.segs.lock().clone();
        for (i, seg) in segs.iter().enumerate().filter(|(_, s)| s.committed > 0) {
            let len = seg.committed.min(TAIL_CHECK) as usize;
            let off = seg.start + seg.committed - len as u64;
            if src(off, len)? != dst(off, len)? {
                log::warn!("segment {i} tail mismatch at {off}; restarting segment");
                ctl.reset_segment(i);
            }
        }
        Ok(())
    }

    /// Transfer every unfinished segment, fanning out over extra
    /// connections when the budget allows. `target` is the remote file
    /// written (upload part) or read (download source).
    fn run_segments(&self, primary: &Conn, job: &Job, ctl: &JobCtl, s: &Settings, b: &StorageBox, target: &str) -> Result<()> {
        let pending: VecDeque<usize> = ctl
            .segs
            .lock()
            .iter()
            .enumerate()
            .filter(|(_, sg)| sg.committed < sg.end - sg.start)
            .map(|(i, _)| i)
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let extra_wanted = pending.len().min(s.streams_per_file).saturating_sub(1);
        let queue = Mutex::new(pending);
        let abort = AtomicBool::new(false);
        let first_err: Mutex<Option<anyhow::Error>> = Mutex::new(None);

        let drain = |c: &Conn| {
            loop {
                let Some(i) = queue.lock().pop_front() else { return };
                if let Err(e) = self.transfer_segment(c, job, ctl, i, target, &abort) {
                    queue.lock().push_front(i);
                    abort.store(true, Ordering::Relaxed);
                    let mut fe = first_err.lock();
                    // Prefer a real error over the Stopped it caused elsewhere.
                    if fe.as_ref().is_none_or(|x| x.downcast_ref::<Stopped>().is_some()) {
                        *fe = Some(e);
                    }
                    return;
                }
            }
        };

        let budget = self.budget(&b.id);
        std::thread::scope(|scope| {
            for _ in 0..extra_wanted {
                let Some(permit) = budget.try_acquire(b.transfer_cap()) else { break };
                let drain = &drain;
                let abort = &abort;
                scope.spawn(move || {
                    let _permit = permit;
                    // A failed extra stream is not fatal: the primary
                    // keeps draining the queue on its own.
                    if let Ok(c) = Conn::connect(b) {
                        if !abort.load(Ordering::Relaxed) {
                            drain(&c);
                        }
                    }
                });
            }
            drain(primary);
        });

        match first_err.into_inner() {
            Some(e) => Err(e),
            None if ctl.stopped() => Err(anyhow!(Stopped)),
            None => Ok(()),
        }
    }

    fn transfer_segment(&self, c: &Conn, job: &Job, ctl: &JobCtl, i: usize, target: &str, abort: &AtomicBool) -> Result<()> {
        let seg = ctl.segs.lock()[i];
        let mut pos = seg.start + seg.committed;
        {
            // Roll back any uncommitted (written but not durable) bytes.
            let mut segs = ctl.segs.lock();
            segs[i].written = segs[i].committed;
        }
        let mut buf = vec![0u8; CHUNK];
        let check = || -> Result<()> {
            if ctl.stopped() || abort.load(Ordering::Relaxed) { Err(anyhow!(Stopped)) } else { Ok(()) }
        };
        match job.direction {
            Direction::Upload => {
                let mut src = File::open(&job.local).map_err(|e| fatal(format!("{LOCAL_MISSING}: {e}")))?;
                src.seek(SeekFrom::Start(pos))?;
                let mut dst = c.open_for_resume(target)?;
                dst.seek(SeekFrom::Start(pos))?;
                while pos < seg.end {
                    check()?;
                    let n = (seg.end - pos).min(CHUNK as u64) as usize;
                    src.read_exact(&mut buf[..n])?;
                    self.limiter.take(n, self.settings.read().bandwidth_limit_mbps);
                    dst.write_all(&buf[..n])?;
                    pos += n as u64;
                    // libssh2 only returns once the server acked the data.
                    let mut segs = ctl.segs.lock();
                    segs[i].written = pos - seg.start;
                    segs[i].committed = pos - seg.start;
                }
                dst.close()?;
            }
            Direction::Download => {
                let part = part_local(&job.local);
                let dst = OpenOptions::new().write(true).open(&part)?;
                let mut src = c.open_read(target)?;
                src.seek(SeekFrom::Start(pos))?;
                let mut last_sync = Instant::now();
                let mut unsynced = 0u64;
                while pos < seg.end {
                    check()?;
                    let n = (seg.end - pos).min(CHUNK as u64) as usize;
                    read_full(&mut src, &mut buf[..n])?;
                    self.limiter.take(n, self.settings.read().bandwidth_limit_mbps);
                    dst.write_all_at(&buf[..n], pos)?;
                    pos += n as u64;
                    unsynced += n as u64;
                    let durable = pos >= seg.end || unsynced >= SYNC_EVERY_BYTES || last_sync.elapsed() >= SYNC_EVERY;
                    if durable {
                        dst.sync_data()?;
                        unsynced = 0;
                        last_sync = Instant::now();
                    }
                    let mut segs = ctl.segs.lock();
                    segs[i].written = pos - seg.start;
                    if durable {
                        segs[i].committed = pos - seg.start;
                    }
                }
            }
        }
        Ok(())
    }

    /// Hash the local file while the storage box hashes its copy.
    fn verify(&self, c: &Conn, id: u64, ctl: &JobCtl, local: &Path, remote_path: &str) -> Result<()> {
        ctl.verifying.store(true, Ordering::Relaxed);
        ctl.verified_bytes.store(0, Ordering::Relaxed);
        if let Some(j) = self.inner.lock().jobs.iter_mut().find(|j| j.id == id) {
            j.state = JobState::Verifying;
        }
        let (l, r) = std::thread::scope(|scope| {
            let h = scope.spawn(|| md5_file(local, ctl));
            let r = c.md5(remote_path);
            (h.join().unwrap_or_else(|_| Err(anyhow!("hash thread panicked"))), r)
        });
        ctl.verifying.store(false, Ordering::Relaxed);
        let (l, r) = (l?, r?);
        if l != r {
            bail!(ChecksumMismatch(format!("Checksum mismatch (local {l}, vault {r}) — transferring again")));
        }
        Ok(())
    }

    /// The job may have been upgraded to "free space" after it started.
    fn offload_requested(&self, id: u64) -> bool {
        self.inner.lock().jobs.iter().any(|j| j.id == id && j.offload)
    }

    /// Record a verified copy and, for "free space" archives, remove the
    /// local file once every target box holds a verified copy of this
    /// exact version (size + mtime) and no other upload still needs it.
    fn after_archive(&self, job: &Job, s: &Settings, verified: bool, size: u64, mtime: u64) -> Result<String> {
        let decision = {
            let mut inner = self.inner.lock();
            let inner = &mut *inner;
            inner.dirty = true;
            if verified {
                let v = inner.verified.entry(job.local.clone()).or_default();
                v.retain(|c| c.box_id != job.box_id);
                v.push(VerifiedCopy { box_id: job.box_id.clone(), size, mtime });
            }
            let (offload, mut targets) = inner
                .jobs
                .iter()
                .find(|j| j.id == job.id)
                .map(|j| (j.offload, j.offload_after.clone()))
                .unwrap_or((job.offload, job.offload_after.clone()));
            if !offload {
                return Ok(String::new());
            }
            if targets.is_empty() {
                targets.push(job.box_id.clone());
            }
            // Other unfinished uploads of this file: hand the clean-up to
            // them rather than pulling the file out from under them.
            let mut waiting_on = Vec::new();
            for o in inner.jobs.iter_mut().filter(|o| {
                o.id != job.id && o.local == job.local && o.direction == Direction::Upload && !o.state.is_terminal()
            }) {
                o.offload = true;
                for t in &targets {
                    if !o.offload_after.contains(t) {
                        o.offload_after.push(t.clone());
                    }
                }
                waiting_on.push(o.box_id.clone());
            }
            let have = inner.verified.get(&job.local);
            let missing: Vec<String> = targets
                .iter()
                .filter(|t| !have.is_some_and(|v| v.iter().any(|c| &c.box_id == *t && c.size == size && c.mtime == mtime)))
                .cloned()
                .collect();
            if missing.is_empty() && waiting_on.is_empty() {
                inner.verified.remove(&job.local);
                None
            } else {
                let mut names: Vec<String> = missing.iter().chain(&waiting_on).map(|id| s.box_by_id(id).map(|b| b.name.clone()).unwrap_or_else(|| id.clone())).collect();
                names.sort();
                names.dedup();
                Some(names)
            }
        };
        if let Some(names) = decision {
            return Ok(format!("Verified · local copy kept until {} {} verified", names.join(" and "), if names.len() == 1 { "is" } else { "are" }));
        }
        match s.offload_mode {
            OffloadMode::Trash => {
                trash::delete(&job.local).map_err(|e| fatal(format!("Archived, but could not move local copy to Trash: {e}")))?;
                Ok("Verified · local copy moved to Trash".into())
            }
            OffloadMode::Delete => {
                fs::remove_file(&job.local).map_err(|e| fatal(format!("Archived, but could not delete local copy: {e}")))?;
                Ok("Verified · local copy removed".into())
            }
        }
    }

    // ─────────────── reporting & persistence ───────────────

    fn reporter(self: Arc<Self>) {
        let mut last: HashMap<u64, (u64, f64)> = HashMap::new();
        let mut history: VecDeque<Sample> = VecDeque::from(vec![Sample::default(); HISTORY_LEN]);
        let mut smooth = Sample::default();
        let mut last_tick = Instant::now();
        let mut last_save = Instant::now();
        let mut session_bytes = 0u64;
        let mut caffeinate: Option<std::process::Child> = None;
        let mut drives_seen_offline: HashSet<PathBuf> = HashSet::new();

        loop {
            std::thread::sleep(TICK);
            let dt = last_tick.elapsed().as_secs_f64().max(0.05);
            last_tick = Instant::now();
            let controls: Vec<(u64, Arc<JobCtl>)> = self.controls.lock().iter().map(|(k, v)| (*k, v.clone())).collect();
            let s = self.settings();

            let mut inner = self.inner.lock();
            let offline_drives = park_offline_jobs(&mut inner, &mut drives_seen_offline);
            let mut up = 0.0;
            let mut down = 0.0;
            let mut live: HashMap<u64, (u64, u64, f64, Vec<SegmentView>)> = HashMap::new();
            for (id, ctl) in &controls {
                let segs = ctl.segs.lock().clone();
                let written: u64 = segs.iter().map(|s| s.written).sum();
                let Some(j) = inner.jobs.iter_mut().find(|j| j.id == *id) else { continue };
                for (dst, src) in j.segments.iter_mut().zip(&segs) {
                    dst.done = src.committed;
                }
                let (prev, ema) = last.get(id).copied().unwrap_or((written, 0.0));
                let delta = written.saturating_sub(prev);
                session_bytes += delta;
                let inst = delta as f64 / dt;
                let ema = if ema == 0.0 { inst } else { ema * 0.7 + inst * 0.3 };
                last.insert(*id, (written, ema));
                match j.direction {
                    Direction::Upload => up += inst,
                    Direction::Download => down += inst,
                }
                let views = segs.iter().map(|s| SegmentView { start: s.start, end: s.end, done: s.written }).collect();
                live.insert(*id, (written, ctl.verified_bytes.load(Ordering::Relaxed), ema, views));
            }
            last.retain(|id, _| live.contains_key(id));
            smooth.up = smooth.up * 0.6 + up * 0.4;
            smooth.down = smooth.down * 0.6 + down * 0.4;
            history.pop_front();
            history.push_back(smooth);

            let now = Instant::now();
            let box_stats: Vec<BoxStats> = {
                let budgets = self.budgets.lock();
                let browse = self.browse.lock();
                s.boxes
                    .iter()
                    .map(|b| {
                        let transfers = budgets.get(&b.id).map(|x| *x.in_use.lock()).unwrap_or(0);
                        // A busy browse slot is in use; an idle one counts if open.
                        let browsing = browse.get(&b.id).is_some_and(|slot| slot.try_lock().is_none_or(|g| g.is_some()));
                        BoxStats { id: b.id.clone(), name: b.name.clone(), connections: transfers + browsing as usize, max_connections: b.max_connections }
                    })
                    .collect()
            };
            let mut stats = Stats {
                up_speed: smooth.up,
                down_speed: smooth.down,
                connections: box_stats.iter().map(|b| b.connections).sum(),
                max_connections: box_stats.iter().map(|b| b.max_connections).sum(),
                boxes: box_stats,
                offline_drives,
                done_total: inner.jobs.iter().filter(|j| j.state == JobState::Done).count(),
                session_bytes,
                history: history.iter().copied().collect(),
                ..Default::default()
            };
            let mut views = Vec::new();
            let mut queued_shown = 0;
            let finished_total = inner.jobs.iter().filter(|j| j.state.is_terminal()).count();
            let mut finished_seen = 0;
            for j in &inner.jobs {
                let (done, verified, speed, segs) = match live.get(&j.id) {
                    Some((w, v, e, segs)) => (*w, *v, *e, segs.clone()),
                    None => (
                        j.committed(),
                        0,
                        0.0,
                        j.segments.iter().map(|s| SegmentView { start: s.start, end: s.end, done: s.done }).collect(),
                    ),
                };
                match j.state {
                    JobState::Running | JobState::Verifying => stats.active += 1,
                    JobState::Queued | JobState::Retrying => stats.queued += 1,
                    JobState::Paused => stats.paused += 1,
                    JobState::Waiting => stats.waiting += 1,
                    JobState::Failed => stats.failed += 1,
                    _ => {}
                }
                if matches!(j.state, JobState::Queued | JobState::Running | JobState::Verifying | JobState::Retrying) {
                    stats.remaining_bytes += j.size.saturating_sub(done);
                }
                // Keep the payload bounded with very large queues.
                let show = match j.state {
                    JobState::Queued => {
                        queued_shown += 1;
                        queued_shown <= 300
                    }
                    s if s.is_terminal() => {
                        finished_seen += 1;
                        finished_total - finished_seen < 200
                    }
                    _ => true,
                };
                if !show {
                    continue;
                }
                let remaining = j.size.saturating_sub(done) as f64;
                views.push(JobView {
                    id: j.id,
                    box_id: j.box_id.clone(),
                    box_name: s.box_by_id(&j.box_id).map(|b| b.name.clone()).unwrap_or_default(),
                    batch: j.batch,
                    batch_label: j.batch_label.clone(),
                    direction: j.direction,
                    name: remote::file_name(&j.remote).to_string(),
                    local: j.local.clone(),
                    remote: j.remote.clone(),
                    size: j.size,
                    done: if j.state == JobState::Done { j.size } else { done },
                    state: j.state,
                    error: j.error.clone(),
                    note: j.note.clone(),
                    attempts: j.attempts,
                    speed,
                    eta: (speed > 1024.0).then(|| remaining / speed),
                    retry_in: j.retry_at.map(|t| t.saturating_duration_since(now).as_secs_f64()),
                    verified,
                    segments: segs,
                    offload: j.offload,
                    finished: j.finished,
                });
            }
            let total_speed = stats.up_speed + stats.down_speed;
            stats.eta = (total_speed > 1024.0 && stats.remaining_bytes > 0).then(|| stats.remaining_bytes as f64 / total_speed);

            let should_save = inner.dirty && (last_save.elapsed() > Duration::from_secs(3) || stats.active == 0);
            let to_save = should_save.then(|| {
                inner.dirty = false;
                serde_json::to_vec(&*inner)
            });
            drop(inner);
            if let Some(Ok(bytes)) = to_save {
                if let Err(e) = config::write_atomic(&jobs_file(), &bytes) {
                    log::error!("saving jobs: {e:#}");
                }
                last_save = Instant::now();
            }

            // Keep the Mac awake while anything is moving.
            let busy = stats.active + stats.queued > 0 && s.prevent_sleep;
            match (busy, caffeinate.is_some()) {
                (true, false) => {
                    caffeinate = std::process::Command::new("/usr/bin/caffeinate")
                        .args(["-i", "-w", &std::process::id().to_string()])
                        .spawn()
                        .ok();
                }
                (false, true) => {
                    if let Some(mut c) = caffeinate.take() {
                        c.kill().ok();
                        c.wait().ok();
                    }
                }
                _ => {}
            }

            (self.hooks.tick)(&Snapshot { jobs: views, stats });
        }
    }
}

#[derive(Debug)]
struct ChecksumMismatch(String);
impl std::fmt::Display for ChecksumMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ChecksumMismatch {}

// ───────────────────────────── helpers ─────────────────────────────

/// Park jobs whose drive is unplugged and wake them when it's back. When a
/// drive reappears, also retry jobs that failed because their files were
/// missing (they were probably on that drive). Returns unplugged drive names.
fn park_offline_jobs(inner: &mut Inner, drives_seen_offline: &mut HashSet<PathBuf>) -> Vec<String> {
    let mut online: HashMap<PathBuf, bool> = HashMap::new();
    let mut offline_drives = Vec::new();
    let mut changed = false;
    for j in inner.jobs.iter_mut() {
        let Some(vol) = volume_root(&j.local) else { continue };
        let up_now = *online.entry(vol.clone()).or_insert_with(|| vol.is_dir());
        match j.state {
            JobState::Queued | JobState::Retrying if !up_now => {
                j.state = JobState::Waiting;
                j.error = Some(offline_message(&j.local));
                j.retry_at = None;
                changed = true;
            }
            JobState::Waiting if up_now => {
                j.state = JobState::Queued;
                j.error = None;
                changed = true;
            }
            JobState::Failed if up_now && drives_seen_offline.contains(&vol) && j.error.as_deref().is_some_and(|e| e.starts_with(LOCAL_MISSING)) => {
                j.state = JobState::Queued;
                j.error = None;
                j.attempts = 0;
                j.finished = None;
                changed = true;
            }
            _ => {}
        }
    }
    inner.dirty |= changed;
    for (vol, up_now) in &online {
        if *up_now {
            drives_seen_offline.remove(vol);
        } else {
            drives_seen_offline.insert(vol.clone());
            offline_drives.push(vol.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        }
    }
    offline_drives.sort();
    offline_drives
}

fn is_running(j: &Job) -> bool {
    matches!(j.state, JobState::Running | JobState::Verifying)
}

fn runnable(j: &Job, now: Instant) -> bool {
    match j.state {
        JobState::Queued => true,
        JobState::Retrying => j.retry_at.is_none_or(|t| t <= now),
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn new_job(
    inner: &mut Inner,
    batch: u64,
    label: &str,
    direction: Direction,
    local: PathBuf,
    remote: String,
    size: u64,
    mtime: u64,
    offload: bool,
    s: &Settings,
) -> Job {
    inner.next_id += 1;
    Job {
        id: inner.next_id,
        box_id: String::new(),
        batch,
        batch_label: label.to_string(),
        direction,
        local,
        remote,
        size,
        mtime,
        offload,
        offload_after: Vec::new(),
        state: JobState::Queued,
        segments: plan_segments(size, s),
        error: None,
        note: None,
        attempts: 0,
        created: now_secs(),
        finished: None,
        retry_at: None,
    }
}

/// Split a file into equal byte ranges, one per stream.
fn plan_segments(size: u64, s: &Settings) -> Vec<Segment> {
    let n = if size >= s.segment_threshold_mb << 20 {
        (s.streams_per_file as u64).min((size / MIN_SEGMENT).max(1))
    } else {
        1
    };
    let step = size.div_ceil(n.max(1)).max(1);
    let mut segs = Vec::new();
    let mut start = 0;
    while start < size {
        let end = (start + step).min(size);
        segs.push(Segment { start, end, done: 0 });
        start = end;
    }
    if segs.is_empty() {
        segs.push(Segment { start: 0, end: 0, done: 0 });
    }
    segs
}

fn batch_label(paths: Vec<String>) -> String {
    match paths.as_slice() {
        [one] => remote::file_name(one.trim_end_matches('/')).to_string(),
        many => format!("{} items", many.len()),
    }
}

fn walk_local(path: &Path, skip_hidden: bool, out: &mut Vec<(PathBuf, u64, u64)>) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(path)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if skip_hidden && name.starts_with('.') {
                continue;
            }
            walk_local(&e.path(), skip_hidden, out)?;
        }
    } else if meta.is_file() && !path.to_string_lossy().ends_with(PART_SUFFIX) {
        out.push((path.to_path_buf(), meta.len(), mtime_of(&meta)));
    }
    Ok(())
}

fn walk_remote(c: &Conn, path: &str, base: &str, skip_hidden: bool, out: &mut Vec<(String, PathBuf, u64, u64)>) -> Result<()> {
    let st = c.stat(path)?.ok_or_else(|| anyhow!("{path} not found in vault"))?;
    if st.is_dir() {
        let mut entries = c.list_dir(path)?;
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        for e in entries {
            if (skip_hidden && e.name.starts_with('.')) || e.name.ends_with(PART_SUFFIX) {
                continue;
            }
            walk_remote(c, &e.path, base, skip_hidden, out)?;
        }
    } else {
        let rel = path.strip_prefix(base).unwrap_or(path).trim_start_matches('/');
        out.push((path.to_string(), PathBuf::from(rel), st.size.unwrap_or(0), st.mtime.unwrap_or(0)));
    }
    Ok(())
}

/// If a different file already sits at `path`, pick `name (2).ext` etc.
/// A same-sized file is assumed to be the same and is left in place.
fn non_clobbering(path: PathBuf, size: u64) -> PathBuf {
    match fs::metadata(&path) {
        Err(_) => path,
        Ok(m) if m.len() == size => path,
        Ok(_) => {
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
            (2..)
                .map(|n| path.with_file_name(format!("{stem} ({n}){ext}")))
                .find(|p| !p.exists())
                .expect("unbounded")
        }
    }
}

fn part_remote(remote: &str) -> String {
    remote::join(remote::parent(remote), &format!(".{}{PART_SUFFIX}", remote::file_name(remote)))
}

fn part_local(local: &Path) -> PathBuf {
    let name = local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    local.with_file_name(format!(".{name}{PART_SUFFIX}"))
}

fn remove_partial(c: Option<&Conn>, job: &Job) {
    match job.direction {
        Direction::Upload => {
            if let Some(c) = c {
                c.unlink(&part_remote(&job.remote)).ok();
            }
        }
        Direction::Download => {
            fs::remove_file(part_local(&job.local)).ok();
        }
    }
}

fn read_local(path: &Path, off: u64, len: usize) -> Result<Vec<u8>> {
    let f = File::open(path)?;
    let mut buf = vec![0u8; len];
    f.read_exact_at(&mut buf, off)?;
    Ok(buf)
}

fn read_remote(c: &Conn, path: &str, off: u64, len: usize) -> Result<Vec<u8>> {
    let mut f = c.open_read(path)?;
    f.seek(SeekFrom::Start(off))?;
    let mut buf = vec![0u8; len];
    read_full(&mut f, &mut buf)?;
    Ok(buf)
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => bail!("connection closed mid-read (unexpected end of file)"),
            n => filled += n,
        }
    }
    Ok(())
}

fn md5_file(path: &Path, ctl: &JobCtl) -> Result<String> {
    let mut f = File::open(path)?;
    let mut h = Md5::new();
    let mut buf = vec![0u8; 8 << 20];
    loop {
        if ctl.stopped() {
            bail!(Stopped);
        }
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        ctl.verified_bytes.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(format!("{:x}", h.finalize()))
}

pub fn free_space(path: &Path) -> u64 {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return 0 };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return 0;
    }
    st.f_bavail as u64 * st.f_frsize as u64
}

pub fn total_space(path: &Path) -> u64 {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return 0 };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return 0;
    }
    st.f_blocks as u64 * st.f_frsize as u64
}

pub fn mtime_of(m: &fs::Metadata) -> u64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn join_note(a: &str, b: &str) -> String {
    if b.is_empty() { a.to_string() } else { format!("{a} · {b}") }
}

fn is_io_like(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some() || c.downcast_ref::<ssh2::Error>().is_some())
}

/// Turn libssh2/io noise into something a producer can act on.
fn humanize_error(e: &anyhow::Error) -> String {
    let full = format!("{e:#}");
    let lower = full.to_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        "Connection stalled — will reconnect and resume".into()
    } else if lower.contains("connecting to") || lower.contains("resolving") || lower.contains("network is unreachable") {
        "Storage box unreachable — waiting for network".into()
    } else if lower.contains("handshake") || lower.contains("connection reset") || lower.contains("broken pipe") || lower.contains("closed") {
        "Connection dropped — will resume where it left off".into()
    } else {
        full
    }
}

pub fn fmt_bytes(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1000.0 && u < units.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 { format!("{b} B") } else { format!("{v:.1} {}", units[u]) }
}

fn jobs_file() -> PathBuf {
    config::data_dir().join("jobs.json")
}

fn load_jobs() -> Inner {
    fs::read(jobs_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(streams: usize) -> Settings {
        Settings { streams_per_file: streams, segment_threshold_mb: 256, ..Default::default() }
    }

    #[test]
    fn small_files_use_one_segment() {
        let segs = plan_segments(10 << 20, &s(4));
        assert_eq!(segs.len(), 1);
        assert_eq!((segs[0].start, segs[0].end), (0, 10 << 20));
    }

    #[test]
    fn large_files_split_and_cover_whole_range() {
        let size = (10u64 << 30) + 12345;
        let segs = plan_segments(size, &s(4));
        assert_eq!(segs.len(), 4);
        assert_eq!(segs[0].start, 0);
        assert_eq!(segs.last().unwrap().end, size);
        for w in segs.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
    }

    #[test]
    fn empty_file_has_one_empty_segment() {
        let segs = plan_segments(0, &s(4));
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].len(), 0);
    }

    #[test]
    fn part_names_are_hidden_siblings() {
        assert_eq!(part_remote("/home/klone/a/b.mov"), "/home/klone/a/.b.mov.klone-part");
        assert_eq!(part_local(Path::new("/x/b.mov")), PathBuf::from("/x/.b.mov.klone-part"));
    }

    /// An engine with no worker threads and an empty queue (never touches
    /// the real jobs file or the network).
    fn bare_engine(s: Settings) -> Engine {
        Engine {
            settings: RwLock::new(s),
            inner: Mutex::new(Inner::default()),
            controls: Mutex::new(HashMap::new()),
            wake: (Mutex::new(()), Condvar::new()),
            budgets: Mutex::new(HashMap::new()),
            browse: Mutex::new(HashMap::new()),
            limiter: Limiter { state: Mutex::new((0.0, Instant::now())) },
            hooks: Hooks { tick: Box::new(|_| {}), batch_done: Box::new(|_| {}) },
        }
    }

    fn two_boxes() -> Settings {
        let mut s = Settings::default();
        let b = StorageBox { user: "u1".into(), ..s.boxes[0].clone() };
        s.boxes = vec![
            StorageBox { id: "a".into(), name: "Alpha".into(), host: "alpha.example".into(), ..b.clone() },
            StorageBox { id: "b".into(), name: "Beta".into(), host: "beta.example".into(), ..b },
        ];
        s.offload_mode = OffloadMode::Delete;
        s.sanitized()
    }

    fn upload_job(e: &Engine, id: u64, box_id: &str, local: &Path, offload_after: &[&str], state: JobState) -> Job {
        let mut j = new_job(&mut e.inner.lock(), 1, "t", Direction::Upload, local.into(), "/r".into(), 4, 0, !offload_after.is_empty(), &e.settings());
        j.id = id;
        j.box_id = box_id.into();
        j.offload_after = offload_after.iter().map(|s| s.to_string()).collect();
        j.state = state;
        e.inner.lock().jobs.push(j.clone());
        j
    }

    fn temp_file(tag: &str) -> (PathBuf, u64, u64) {
        let dir = std::env::temp_dir().join(format!("klone-unit-{tag}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("clip.mov");
        fs::write(&f, b"data").unwrap();
        let m = fs::metadata(&f).unwrap();
        (f, m.len(), mtime_of(&m))
    }

    #[test]
    fn free_space_waits_for_every_chosen_box() {
        let e = bare_engine(two_boxes());
        let (f, size, mtime) = temp_file("both");
        let a = upload_job(&e, 1, "a", &f, &["a", "b"], JobState::Running);
        let b = upload_job(&e, 2, "b", &f, &["a", "b"], JobState::Queued);
        let s = e.settings();

        let note = e.after_archive(&a, &s, true, size, mtime).unwrap();
        assert!(f.exists(), "freed before Beta had a copy");
        assert!(note.contains("kept until Beta"), "{note}");
        e.inner.lock().jobs[0].state = JobState::Done;

        e.inner.lock().jobs[1].state = JobState::Running;
        let note = e.after_archive(&b, &s, true, size, mtime).unwrap();
        assert!(!f.exists(), "should be freed once both are verified: {note}");
        assert!(e.inner.lock().verified.is_empty());
        fs::remove_dir_all(f.parent().unwrap()).ok();
    }

    #[test]
    fn changed_file_is_never_freed_on_a_stale_verification() {
        let e = bare_engine(two_boxes());
        let (f, size, mtime) = temp_file("stale");
        let a = upload_job(&e, 1, "a", &f, &["a", "b"], JobState::Running);
        let b = upload_job(&e, 2, "b", &f, &["a", "b"], JobState::Running);
        let s = e.settings();
        e.after_archive(&a, &s, true, size, mtime).unwrap();
        e.inner.lock().jobs[0].state = JobState::Done;
        // Beta verified a newer version of the file than Alpha did.
        let note = e.after_archive(&b, &s, true, size, mtime + 60).unwrap();
        assert!(f.exists(), "{note}");
        assert!(note.contains("Alpha"), "{note}");
        fs::remove_dir_all(f.parent().unwrap()).ok();
    }

    #[test]
    fn other_unfinished_upload_takes_over_the_clean_up() {
        let e = bare_engine(two_boxes());
        let (f, size, mtime) = temp_file("handoff");
        // "Archive & free space" to Alpha while a plain archive to Beta runs.
        let a = upload_job(&e, 1, "a", &f, &["a"], JobState::Running);
        let _b = upload_job(&e, 2, "b", &f, &[], JobState::Running);
        let note = e.after_archive(&a, &e.settings(), true, size, mtime).unwrap();
        assert!(f.exists(), "pulled the file out from under Beta's upload: {note}");
        let inner = e.inner.lock();
        let b = inner.jobs.iter().find(|j| j.id == 2).unwrap();
        assert!(b.offload && b.offload_after == vec!["a".to_string()], "Beta should now free the file when done");
        drop(inner);
        fs::remove_dir_all(f.parent().unwrap()).ok();
    }

    #[test]
    fn archive_queues_one_job_per_box_and_upgrades_duplicates() {
        let e = bare_engine(two_boxes());
        let (f, _, _) = temp_file("enqueue");
        assert_eq!(e.archive(&[f.clone()], false, &["a".into()]).unwrap(), 1);
        // Again, to both boxes with free space: Alpha is upgraded, Beta added.
        assert_eq!(e.archive(&[f.clone()], true, &["a".into(), "b".into()]).unwrap(), 1);
        let inner = e.inner.lock();
        assert_eq!(inner.jobs.len(), 2);
        for j in &inner.jobs {
            assert!(j.offload, "{} not upgraded", j.box_id);
            assert_eq!(j.offload_after, vec!["a".to_string(), "b".to_string()]);
        }
        drop(inner);
        fs::remove_dir_all(f.parent().unwrap()).ok();
    }

    #[test]
    fn unplugged_drive_parks_jobs_instead_of_failing() {
        let e = bare_engine(two_boxes());
        let gone = Path::new("/Volumes/klone-test-unplugged-x9/Shoot/a.braw");
        upload_job(&e, 1, "a", gone, &[], JobState::Queued);
        upload_job(&e, 2, "a", gone, &[], JobState::Retrying);
        let mut seen = HashSet::new();
        let mut inner = e.inner.lock();
        let names = park_offline_jobs(&mut inner, &mut seen);
        assert_eq!(names, vec!["klone-test-unplugged-x9".to_string()]);
        assert!(inner.jobs.iter().all(|j| j.state == JobState::Waiting));
        assert!(inner.jobs[0].error.as_deref().unwrap().contains("isn't connected"));
        assert!(!runnable(&inner.jobs[0], Instant::now()), "waiting jobs must not be picked up");
        // A local (non-/Volumes) path is never parked.
        drop(inner);
        upload_job(&e, 3, "a", Path::new("/Users/x/a.mov"), &[], JobState::Queued);
        let mut inner = e.inner.lock();
        park_offline_jobs(&mut inner, &mut seen);
        assert_eq!(inner.jobs[2].state, JobState::Queued);
    }

    #[test]
    fn detects_external_drive_paths() {
        assert_eq!(volume_root(Path::new("/Volumes/Shoot_01/Project/a.braw")), Some(PathBuf::from("/Volumes/Shoot_01")));
        assert_eq!(volume_root(Path::new("/Users/x/a.mov")), None);
        assert!(drive_online(Path::new("/Users/x/a.mov")));
        assert!(!drive_online(Path::new("/Volumes/klone-test-not-mounted-x9/a.mov")));
    }

    #[test]
    fn budget_never_exceeds_cap() {
        let b = Budget::new();
        let p1 = b.try_acquire(2).unwrap();
        let _p2 = b.try_acquire(2).unwrap();
        assert!(b.try_acquire(2).is_none());
        drop(p1);
        assert!(b.try_acquire(2).is_some());
    }
}

/// End-to-end tests against the real storage box. Run with:
/// `KLONE_DATA_DIR=$(mktemp -d) cargo test -- --ignored --nocapture`
#[cfg(test)]
mod live {
    use super::*;

    fn wait_for(engine: &Engine, pred: impl Fn(&Job) -> bool, secs: u64) -> Job {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            {
                let inner = engine.inner.lock();
                if let Some(j) = inner.jobs.iter().rev().find(|j| pred(j)) {
                    return j.clone();
                }
            }
            assert!(Instant::now() < deadline, "timed out; jobs: {:#?}", engine.inner.lock().jobs);
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    #[test]
    #[ignore]
    fn roundtrip_with_pause_resume() {
        assert!(std::env::var_os("KLONE_DATA_DIR").is_some(), "set KLONE_DATA_DIR to a temp dir");
        let tmp = std::env::temp_dir().join(format!("klone-live-{}", now_secs()));
        let lib = tmp.join("lib");
        fs::create_dir_all(lib.join("shoot")).unwrap();
        let src = lib.join("shoot/clip.bin");
        // 300 MB of pseudo-random data → segmented across streams.
        {
            let mut f = File::create(&src).unwrap();
            let mut x: u64 = 0x9E3779B97F4A7C15;
            let mut buf = vec![0u8; 1 << 20];
            for _ in 0..300 {
                for b in buf.chunks_mut(8) {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    b.copy_from_slice(&x.to_le_bytes());
                }
                f.write_all(&buf).unwrap();
            }
        }
        let mut s = Settings::default();
        s.boxes = vec![config::live_test_box()];
        s.boxes[0].remote_root = format!("/home/klone-test-{}", now_secs());
        s.boxes[0].max_connections = 5;
        s.libraries = vec![config::Library { name: "Lib".into(), path: lib.clone() }];
        s.segment_threshold_mb = 128;
        s.prevent_sleep = false;
        let s = s.sanitized();
        let engine = Engine::start(s.clone(), Hooks { tick: Box::new(|_| {}), batch_done: Box::new(|_| {}) });

        let bid = s.boxes[0].id.clone();
        assert_eq!(engine.archive(&[lib.join("shoot")], false, &[bid.clone()]).unwrap(), 1);
        let job = wait_for(&engine, |j| j.state == JobState::Running, 60);
        assert!(job.segments.len() > 1, "expected segmented upload");
        // Let some data flow, then pause and make sure progress was kept.
        wait_for(&engine, |j| j.committed() > 20 << 20, 300);
        engine.pause(&[job.id]);
        let paused = wait_for(&engine, |j| j.state == JobState::Paused, 60);
        let kept = paused.committed();
        println!("paused with {} committed", fmt_bytes(kept));
        assert!(kept > 0);
        engine.resume(&[job.id]);
        let done = wait_for(&engine, |j| j.id == job.id && j.state.is_terminal(), 1800);
        assert_eq!(done.state, JobState::Done, "{:?}", done.error);
        println!("upload done: {:?}", done.note);

        // Download into a fresh folder and compare bytes.
        let out = tmp.join("out");
        let remote = s.boxes[0].remote_path_for(&s.libraries, &src);
        assert_eq!(engine.retrieve(&bid, &[remote.clone()], Some(out.clone())).unwrap(), 1);
        let d = wait_for(&engine, |j| j.direction == Direction::Download && j.state.is_terminal(), 1800);
        assert_eq!(d.state, JobState::Done, "{:?}", d.error);
        let a = fs::read(&src).unwrap();
        let b = fs::read(out.join("clip.bin")).unwrap();
        assert!(a == b, "downloaded bytes differ");

        // Clean up the remote test tree.
        engine
            .with_conn(&bid, |c| {
                c.unlink(&remote)?;
                c.rmdir(remote::parent(&remote))?;
                c.rmdir(remote::parent(remote::parent(&remote)))?;
                c.rmdir(&s.boxes[0].remote_root)
            })
            .unwrap();
        fs::remove_dir_all(&tmp).ok();
    }

    /// Archive & free space to two "boxes" (two roots on the same storage
    /// box): the local file must survive until both copies are verified.
    #[test]
    #[ignore]
    fn offload_to_two_boxes_waits_for_both() {
        assert!(std::env::var_os("KLONE_DATA_DIR").is_some(), "set KLONE_DATA_DIR to a temp dir");
        let tmp = std::env::temp_dir().join(format!("klone-live2-{}", now_secs()));
        let lib = tmp.join("lib");
        fs::create_dir_all(&lib).unwrap();
        let src = lib.join("clip.bin");
        let data: Vec<u8> = (0..(40u32 << 20)).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        fs::write(&src, &data).unwrap();

        let stamp = now_secs();
        let mut s = Settings::default();
        let first = config::live_test_box();
        s.boxes = vec![
            StorageBox { id: "a".into(), name: "Alpha".into(), remote_root: format!("/home/klone-test-a-{stamp}"), max_connections: 4, ..first.clone() },
            StorageBox { id: "b".into(), name: "Beta".into(), remote_root: format!("/home/klone-test-b-{stamp}"), max_connections: 4, ..first },
        ];
        s.libraries = vec![config::Library { name: "Lib".into(), path: lib.clone() }];
        s.offload_mode = OffloadMode::Delete;
        s.prevent_sleep = false;
        s.segment_threshold_mb = 16;
        let s = s.sanitized();
        let engine = Engine::start(s.clone(), Hooks { tick: Box::new(|_| {}), batch_done: Box::new(|_| {}) });

        // Hold Beta back so Alpha finishes first.
        assert_eq!(engine.archive(&[src.clone()], true, &["a".into(), "b".into()]).unwrap(), 2);
        let beta = engine.inner.lock().jobs.iter().find(|j| j.box_id == "b").unwrap().id;
        engine.pause(&[beta]);
        let a = wait_for(&engine, |j| j.box_id == "a" && j.state.is_terminal(), 600);
        assert_eq!(a.state, JobState::Done, "{:?}", a.error);
        println!("alpha: {:?}", a.note);
        assert!(src.exists(), "local copy removed before Beta was verified");
        assert!(a.note.as_deref().unwrap_or("").contains("kept until Beta"), "{:?}", a.note);

        engine.resume(&[beta]);
        let b = wait_for(&engine, |j| j.box_id == "b" && j.state.is_terminal(), 600);
        assert_eq!(b.state, JobState::Done, "{:?}", b.error);
        println!("beta: {:?}", b.note);
        assert!(!src.exists(), "local copy should be freed once both boxes verified");

        for bx in &s.boxes {
            let remote = bx.remote_path_for(&s.libraries, &src);
            let (size, sum) = engine.with_conn(&bx.id, |c| Ok((c.stat(&remote)?.and_then(|st| st.size), c.md5(&remote)?))).unwrap();
            assert_eq!(size, Some(data.len() as u64));
            assert_eq!(sum, format!("{:x}", Md5::digest(&data)));
            engine
                .with_conn(&bx.id, |c| {
                    c.unlink(&remote)?;
                    c.rmdir(remote::parent(&remote))?;
                    c.rmdir(&bx.remote_root)
                })
                .unwrap();
        }
        fs::remove_dir_all(&tmp).ok();
    }
}
