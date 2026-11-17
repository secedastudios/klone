//! Thin, blocking SSH/SFTP layer over libssh2.
//!
//! Every transfer stream owns its own [`Conn`]: SSH multiplexing over one
//! TCP connection is window-limited, so separate connections are what
//! actually buys throughput on high-latency links to the storage box.

use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use ssh2::{CheckResult, ErrorCode, KnownHostFileKind, OpenFlags, OpenType, Session, Sftp};

use crate::config::StorageBox;

/// libssh2's SFTP status code for "no such file".
const SFTP_NO_SUCH_FILE: i32 = 2;

pub struct Conn {
    pub sess: Session,
    pub sftp: Sftp,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u64,
}

impl Conn {
    pub fn connect(s: &StorageBox) -> Result<Self> {
        if !s.is_configured() {
            bail!("“{}” isn't set up yet — add its server and username in Settings", s.name);
        }
        let addr = (s.host.as_str(), s.port)
            .to_socket_addrs()
            .with_context(|| format!("resolving {}", s.host))?
            .next()
            .ok_or_else(|| anyhow!("{} did not resolve", s.host))?;
        let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(15))
            .with_context(|| format!("connecting to {}:{}", s.host, s.port))?;
        tcp.set_nodelay(true).ok();

        let mut sess = Session::new()?;
        sess.set_tcp_stream(tcp);
        // Any single blocking libssh2 call that stalls this long means the
        // link is dead: fail fast and let the engine reconnect + resume.
        sess.set_timeout(45_000);
        sess.set_compress(false);
        sess.handshake().context("SSH handshake")?;
        check_host_key(&sess, s)?;

        // The configured key first, so "Test connection" reflects that key;
        // ssh-agent is the fallback (e.g. for passphrase-protected keys).
        let file_err = if s.key_path.is_file() {
            sess.userauth_pubkey_file(&s.user, None, &s.key_path, None).err()
        } else {
            None
        };
        if !sess.authenticated() {
            let _ = sess.userauth_agent(&s.user);
        }
        if !sess.authenticated() {
            if !s.key_path.is_file() {
                bail!("No SSH key at {} — create one in Settings", s.key_path.display());
            }
            match file_err {
                Some(e) if matches!(e.code(), ssh2::ErrorCode::Session(-18 | -19)) => bail!(
                    "{} didn't accept this key. Add its public key to the box's authorized keys (Settings → Copy public key).", s.name
                ),
                Some(e) => bail!("Couldn't use the key at {}: {e}", s.key_path.display()),
                None => bail!("authentication rejected for {}", s.user),
            }
        }
        sess.set_keepalive(true, 20);
        let sftp = sess.sftp().context("starting SFTP subsystem")?;
        Ok(Self { sess, sftp })
    }

    /// Stat a remote path; `Ok(None)` when it does not exist.
    pub fn stat(&self, path: &str) -> Result<Option<ssh2::FileStat>> {
        match self.sftp.stat(Path::new(path)) {
            Ok(st) => Ok(Some(st)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("stat {path}")),
        }
    }

    pub fn list_dir(&self, path: &str) -> Result<Vec<RemoteEntry>> {
        let entries = match self.sftp.readdir(Path::new(path)) {
            Ok(e) => e,
            Err(e) if is_not_found(&e) => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("listing {path}")),
        };
        Ok(entries
            .into_iter()
            .filter_map(|(p, st)| {
                let name = p.file_name()?.to_string_lossy().into_owned();
                if name == "." || name == ".." {
                    return None;
                }
                Some(RemoteEntry {
                    path: join(path, &name),
                    name,
                    is_dir: st.is_dir(),
                    size: st.size.unwrap_or(0),
                    mtime: st.mtime.unwrap_or(0),
                })
            })
            .collect())
    }

    /// `mkdir -p`. Tolerates races with other streams creating the same dirs.
    pub fn mkdir_p(&self, path: &str) -> Result<()> {
        let mut cur = String::new();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur.push('/');
            cur.push_str(part);
            if cur == "/home" {
                continue; // storage box chroot root, always exists
            }
            match self.stat(&cur)? {
                Some(st) if st.is_dir() => {}
                Some(_) => bail!("{cur} exists and is not a directory"),
                None => {
                    if let Err(e) = self.sftp.mkdir(Path::new(&cur), 0o755) {
                        if !matches!(self.stat(&cur)?, Some(st) if st.is_dir()) {
                            return Err(e).with_context(|| format!("mkdir {cur}"));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Open (creating if needed) a remote file for positioned writes
    /// without truncating it, so partial uploads can be resumed.
    pub fn open_for_resume(&self, path: &str) -> Result<ssh2::File> {
        self.sftp
            .open_mode(Path::new(path), OpenFlags::WRITE | OpenFlags::CREATE, 0o644, OpenType::File)
            .with_context(|| format!("opening {path} for writing"))
    }

    pub fn open_read(&self, path: &str) -> Result<ssh2::File> {
        self.sftp.open(Path::new(path)).with_context(|| format!("opening {path}"))
    }

    pub fn truncate(&self, path: &str, size: u64) -> Result<()> {
        let st = ssh2::FileStat { size: Some(size), uid: None, gid: None, perm: None, atime: None, mtime: None };
        self.sftp.setstat(Path::new(path), st).with_context(|| format!("truncating {path}"))
    }

    pub fn set_mtime(&self, path: &str, mtime: u64) -> Result<()> {
        let st = ssh2::FileStat { size: None, uid: None, gid: None, perm: None, atime: Some(mtime), mtime: Some(mtime) };
        self.sftp.setstat(Path::new(path), st).with_context(|| format!("setting mtime on {path}"))
    }

    /// The login directory (`/home` on a Hetzner Storage Box).
    pub fn home(&self) -> Result<String> {
        let p = self.sftp.realpath(Path::new(".")).context("finding the home folder")?;
        Ok(p.to_string_lossy().into_owned())
    }

    /// Whole small file as text; `None` if it doesn't exist.
    pub fn read_text(&self, path: &str) -> Result<Option<String>> {
        let mut f = match self.sftp.open(Path::new(path)) {
            Ok(f) => f,
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("opening {path}")),
        };
        let mut s = String::new();
        f.read_to_string(&mut s).with_context(|| format!("reading {path}"))?;
        Ok(Some(s))
    }

    /// Replace a small file: write a temp sibling, read it back to check,
    /// then move it into place. Owner-only permissions, as sshd expects.
    pub fn write_text(&self, path: &str, text: &str) -> Result<()> {
        use std::io::Write;
        let tmp = format!("{path}.klone-tmp");
        {
            let mut f = self
                .sftp
                .open_mode(Path::new(&tmp), OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE, 0o600, OpenType::File)
                .with_context(|| format!("creating {tmp}"))?;
            f.write_all(text.as_bytes()).with_context(|| format!("writing {tmp}"))?;
        }
        if self.read_text(&tmp)?.as_deref() != Some(text) {
            self.unlink(&tmp).ok();
            bail!("{tmp} didn't read back correctly");
        }
        self.replace(&tmp, path)
    }

    /// Atomically move `from` over `to`, replacing any existing file.
    pub fn replace(&self, from: &str, to: &str) -> Result<()> {
        if self.stat(to)?.is_some() {
            self.sftp.unlink(Path::new(to)).with_context(|| format!("removing old {to}"))?;
        }
        self.sftp
            .rename(Path::new(from), Path::new(to), None)
            .with_context(|| format!("renaming {from} -> {to}"))
    }

    pub fn unlink(&self, path: &str) -> Result<()> {
        self.sftp.unlink(Path::new(path)).with_context(|| format!("deleting {path}"))
    }

    pub fn rmdir(&self, path: &str) -> Result<()> {
        self.sftp.rmdir(Path::new(path)).with_context(|| format!("removing folder {path}"))
    }

    /// Run a command in the storage box's restricted shell.
    pub fn exec(&self, cmd: &str) -> Result<String> {
        self.exec_for(cmd, Duration::from_secs(45))
    }

    /// Run a command that may stay silent for a long time (hashing tens of
    /// GB). libssh2 reports "timed out" whenever a blocking wait outlives
    /// the keepalive interval, even with the session timeout off, so wait
    /// in short slices and send a keepalive between them (which also keeps
    /// NAT routers from dropping the idle connection) until `limit`.
    pub fn exec_for(&self, cmd: &str, limit: Duration) -> Result<String> {
        let deadline = std::time::Instant::now() + limit;
        let mut ch = self.sess.channel_session()?;
        ch.exec(cmd).with_context(|| format!("running `{cmd}`"))?;
        let patient = |r: std::io::Result<usize>| -> Result<Option<usize>> {
            match r {
                Ok(n) => Ok(Some(n)),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock) => {
                    if std::time::Instant::now() >= deadline {
                        bail!("`{cmd}` timed out after {}s", limit.as_secs());
                    }
                    self.sess.keepalive_send().ok();
                    Ok(None)
                }
                Err(e) => Err(e.into()),
            }
        };
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match patient(ch.read(&mut buf))? {
                Some(0) => break,
                Some(n) => out.extend_from_slice(&buf[..n]),
                None => {}
            }
        }
        let mut err = Vec::new();
        loop {
            match patient(ch.stderr().read(&mut buf)) {
                Ok(Some(0)) | Err(_) => break,
                Ok(Some(n)) => err.extend_from_slice(&buf[..n]),
                Ok(None) => {}
            }
        }
        ch.wait_close().ok();
        let code = ch.exit_status().unwrap_or(-1);
        let out = String::from_utf8_lossy(&out).into_owned();
        if code != 0 {
            bail!("`{cmd}` exited with {code}: {}", String::from_utf8_lossy(&err).trim());
        }
        Ok(out)
    }

    /// Server-side MD5 — the storage box hashes at disk speed, no download needed.
    pub fn md5(&self, path: &str) -> Result<String> {
        // The box hashes at ~600 MB/s; allow for a busy box and huge files.
        // Short waits so keepalives go out while md5sum is silent.
        self.sess.set_timeout(10_000);
        let out = self.exec_for(&format!("md5sum {}", shell_quote(path)), Duration::from_secs(6 * 3600));
        self.sess.set_timeout(45_000);
        let out = out?;
        let hash = out.split_whitespace().next().unwrap_or("").trim_start_matches('\\');
        if hash.len() != 32 {
            bail!("unexpected md5sum output: {out}");
        }
        Ok(hash.to_ascii_lowercase())
    }

    /// (total, used) bytes for the storage box.
    pub fn usage(&self) -> Result<(u64, u64)> {
        let out = self.exec("df -B1")?;
        let line = out.lines().nth(1).ok_or_else(|| anyhow!("unexpected df output"))?;
        let cols: Vec<&str> = line.split_whitespace().collect();
        let total = cols.get(1).and_then(|v| v.parse().ok()).ok_or_else(|| anyhow!("bad df total"))?;
        let used = cols.get(2).and_then(|v| v.parse().ok()).ok_or_else(|| anyhow!("bad df used"))?;
        Ok((total, used))
    }
}

fn check_host_key(sess: &Session, s: &StorageBox) -> Result<()> {
    let (key, kind) = sess.host_key().ok_or_else(|| anyhow!("server sent no host key"))?;
    let mut kh = sess.known_hosts()?;
    let file = dirs::home_dir().unwrap_or_default().join(".ssh/known_hosts");
    if file.exists() {
        kh.read_file(&file, KnownHostFileKind::OpenSSH).ok();
    }
    match kh.check_port(&s.host, s.port, key) {
        CheckResult::Match => Ok(()),
        CheckResult::Mismatch => bail!(
            "host key for {} has CHANGED — refusing to connect (check ~/.ssh/known_hosts)",
            s.host
        ),
        // Trust on first use, exactly like `ssh` with accept-new.
        CheckResult::NotFound | CheckResult::Failure => {
            let name = if s.port == 22 { s.host.clone() } else { format!("[{}]:{}", s.host, s.port) };
            if kh.add(&name, key, "added by Klone", kind.into()).is_ok() {
                kh.write_file(&file, KnownHostFileKind::OpenSSH).ok();
            }
            Ok(())
        }
    }
}

pub fn is_not_found(e: &ssh2::Error) -> bool {
    matches!(e.code(), ErrorCode::SFTP(SFTP_NO_SUCH_FILE))
}

pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

pub fn parent(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => "/",
    }
}

pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// POSIX single-quote escaping (verified against the storage box shell).
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_helpers() {
        assert_eq!(join("/home", "a"), "/home/a");
        assert_eq!(join("/", "a"), "/a");
        assert_eq!(parent("/home/klone/a.mov"), "/home/klone");
        assert_eq!(parent("/a"), "/");
        assert_eq!(file_name("/home/klone/a.mov"), "a.mov");
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
    }
}


