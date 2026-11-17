//! SSH key helpers for first-time setup: inspect the configured key,
//! generate a fresh ed25519 key, and put the public half on the clipboard
//! so it can be pasted into the storage box's authorized keys, and edit
//! a box's `authorized_keys` so other people's keys can be installed.
//!
//! Uses the system `ssh-keygen` so keys are standard OpenSSH files that
//! also work with `ssh`, `sftp`, and ssh-agent.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

const SSH_KEYGEN: &str = "/usr/bin/ssh-keygen";
const KEY_NAME: &str = "klone_ed25519";

#[derive(Serialize, Debug)]
pub struct KeyInfo {
    pub path: PathBuf,
    /// The private key file exists.
    pub exists: bool,
    /// One-line OpenSSH public key (`ssh-ed25519 AAAA… comment`).
    pub public_key: Option<String>,
    /// `SHA256:…` fingerprint, as shown by `ssh-keygen -l`.
    pub fingerprint: Option<String>,
    /// Key type, e.g. `ED25519` or `RSA`.
    pub kind: Option<String>,
}

fn ssh_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")).join(".ssh")
}

/// Best default key for a fresh install: the Klone key if one exists,
/// then the usual OpenSSH names, else where `generate` would put one.
pub fn default_key_path() -> PathBuf {
    let dir = ssh_dir();
    [KEY_NAME, "id_ed25519", "id_ecdsa", "id_rsa"]
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
        .unwrap_or_else(|| dir.join(KEY_NAME))
}

fn pub_path(key: &Path) -> PathBuf {
    let mut s = key.as_os_str().to_owned();
    s.push(".pub");
    PathBuf::from(s)
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.stdin(Stdio::null()).output().context("running ssh-keygen")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Public key text: the `.pub` file, or derived from the private key
/// (works for unencrypted keys) when the `.pub` is missing.
fn public_key(key: &Path) -> Option<String> {
    // A `.pub` file itself (e.g. one a teammate sent) is read as-is.
    let file = if key.extension().is_some_and(|e| e == "pub") { key.to_path_buf() } else { pub_path(key) };
    if let Ok(s) = std::fs::read_to_string(file) {
        let line = s.lines().next().unwrap_or("").trim();
        if !line.is_empty() {
            return Some(line.to_string());
        }
    }
    if !key.is_file() {
        return None;
    }
    // -P "" makes an encrypted key fail fast instead of prompting.
    run(Command::new(SSH_KEYGEN).args(["-y", "-P", ""]).arg("-f").arg(key)).ok().filter(|s| !s.is_empty())
}

pub fn info(key: &Path) -> KeyInfo {
    let public_key = public_key(key);
    let (fingerprint, kind) = match &public_key {
        Some(pk) => fingerprint(pk).map(|(f, k)| (Some(f), Some(k))).unwrap_or((None, None)),
        None => (None, None),
    };
    KeyInfo { path: key.to_path_buf(), exists: key.is_file(), public_key, fingerprint, kind }
}

/// `ssh-keygen -lf -` reads the public key from stdin and prints
/// `256 SHA256:abc… comment (ED25519)`.
fn fingerprint(public_key: &str) -> Result<(String, String)> {
    let mut child = Command::new(SSH_KEYGEN)
        .args(["-l", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ssh-keygen")?;
    child.stdin.take().unwrap().write_all(public_key.as_bytes())?;
    let out = child.wait_with_output()?;
    let line = String::from_utf8_lossy(&out.stdout);
    let fp = line.split_whitespace().nth(1).ok_or_else(|| anyhow!("unreadable public key"))?.to_string();
    let kind = line.rsplit_once('(').map(|(_, k)| k.trim_end_matches([')', '\n']).to_string()).unwrap_or_default();
    Ok((fp, kind))
}

/// Create a new passwordless ed25519 key in `~/.ssh`, never overwriting
/// an existing file. Returns the private key path.
pub fn generate() -> Result<PathBuf> {
    let dir = ssh_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok();
    }
    let path = (1..)
        .map(|i| if i == 1 { dir.join(KEY_NAME) } else { dir.join(format!("{KEY_NAME}_{i}")) })
        .find(|p| !p.exists() && !pub_path(p).exists())
        .unwrap();
    run(Command::new(SSH_KEYGEN)
        .args(["-q", "-t", "ed25519", "-N", ""])
        .arg("-C")
        .arg(format!("klone@{}", short_hostname()))
        .arg("-f")
        .arg(&path))?;
    Ok(path)
}

fn short_hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: buf is valid for its full length; gethostname NUL-terminates on success.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let name = if ok {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    };
    let short = name.split('.').next().unwrap_or("").trim();
    if short.is_empty() { "mac".into() } else { short.to_string() }
}

/// One entry of a box's `authorized_keys`, as shown in Settings.
#[derive(Serialize, Clone, Debug)]
pub struct AuthorizedKey {
    /// Base64 key blob — the identity used to remove it.
    pub id: String,
    pub kind: String,
    pub comment: String,
    pub fingerprint: String,
    /// The key this Mac's Klone signs in with.
    pub mine: bool,
}

fn is_key_type(t: &str) -> bool {
    matches!(t, "ssh-ed25519" | "ssh-rsa" | "ssh-dss") || t.starts_with("ecdsa-sha2-") || t.starts_with("sk-")
}

/// (type, base64 blob, comment) of an `authorized_keys` or `.pub` line,
/// skipping any options prefix (`from="…",no-pty ssh-ed25519 AAAA… c`).
pub fn parse_key_line(line: &str) -> Option<(String, String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let toks: Vec<&str> = line.split_whitespace().collect();
    let i = toks.iter().position(|t| is_key_type(t))?;
    let blob = toks.get(i + 1)?;
    if blob.len() < 16 || !blob.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)) {
        return None;
    }
    Some((toks[i].to_string(), blob.to_string(), toks[i + 2..].join(" ")))
}

/// Base64 blob of the key this Mac signs in with, if readable.
pub fn own_blob(key_path: &Path) -> Option<String> {
    info(key_path).public_key.as_deref().and_then(parse_key_line).map(|(_, b, _)| b)
}

/// Turn pasted text into one clean `type blob comment` line, or explain
/// what's wrong with it. Accepts OpenSSH and RFC 4716 (`---- BEGIN SSH2`) keys.
pub fn normalize_new_key(text: &str) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        bail!("Paste a public key first");
    }
    if text.contains("PRIVATE KEY") {
        bail!("That's a private key — never share it. Ask for the .pub file instead.");
    }
    let text = if text.starts_with("---- BEGIN SSH2 PUBLIC KEY") {
        // `ssh-keygen -i` won't read stdin, so go through a temp file.
        let tmp = std::env::temp_dir().join(format!("klone-import-{}-{}.pub", std::process::id(), now_nanos()));
        std::fs::write(&tmp, text)?;
        let r = run(Command::new(SSH_KEYGEN).args(["-i", "-m", "RFC4716", "-f"]).arg(&tmp));
        std::fs::remove_file(&tmp).ok();
        r.map_err(|_| anyhow!("Couldn't read that SSH2 public key"))?
    } else {
        text.to_string()
    };
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    if lines.len() != 1 {
        bail!("Paste one public key at a time");
    }
    let (kind, blob, comment) =
        parse_key_line(lines[0]).ok_or_else(|| anyhow!("That doesn't look like an SSH public key (it should start with ssh-ed25519, ssh-rsa or ecdsa-…)"))?;
    let line = if comment.is_empty() { format!("{kind} {blob}") } else { format!("{kind} {blob} {comment}") };
    fingerprint(&line).map_err(|_| anyhow!("That public key is damaged or incomplete"))?;
    Ok(line)
}

/// Keys listed in `content`, with fingerprints.
pub fn list_authorized(content: &str, own: Option<&str>) -> Vec<AuthorizedKey> {
    content
        .lines()
        .filter_map(parse_key_line)
        .map(|(kind, blob, comment)| AuthorizedKey {
            fingerprint: fingerprint(&format!("{kind} {blob}")).map(|(f, _)| f).unwrap_or_default(),
            mine: own == Some(blob.as_str()),
            id: blob,
            kind,
            comment,
        })
        .collect()
}

/// `content` with `line` appended, or `None` if that key is already there.
pub fn add_authorized(content: &str, line: &str) -> Option<String> {
    let (_, blob, _) = parse_key_line(line)?;
    if content.lines().filter_map(parse_key_line).any(|(_, b, _)| b == blob) {
        return None;
    }
    let mut out = content.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(line);
    out.push('\n');
    Some(out)
}

/// `content` without the key `blob`; every other line is kept verbatim.
pub fn remove_authorized(content: &str, blob: &str) -> String {
    let mut out: String = content
        .lines()
        .filter(|l| parse_key_line(l).is_none_or(|(_, b, _)| b != blob))
        .collect::<Vec<_>>()
        .join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

pub fn copy_to_clipboard(text: &str) -> Result<()> {
    let mut child = Command::new("/usr/bin/pbcopy").stdin(Stdio::piped()).spawn().context("running pbcopy")?;
    child.stdin.take().unwrap().write_all(text.as_bytes())?;
    if !child.wait()?.success() {
        bail!("pbcopy failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_for_generated_key() {
        let dir = std::env::temp_dir().join(format!("klone-keytest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("k");
        run(Command::new(SSH_KEYGEN).args(["-q", "-t", "ed25519", "-N", "", "-C", "t@x", "-f"]).arg(&key)).unwrap();

        let i = info(&key);
        assert!(i.exists);
        assert!(i.public_key.as_deref().unwrap().starts_with("ssh-ed25519 "));
        assert!(i.fingerprint.as_deref().unwrap().starts_with("SHA256:"));
        assert_eq!(i.kind.as_deref(), Some("ED25519"));

        // Missing .pub: derived from the private key.
        std::fs::remove_file(pub_path(&key)).unwrap();
        assert!(info(&key).public_key.unwrap().starts_with("ssh-ed25519 "));

        let none = info(&dir.join("nope"));
        assert!(!none.exists && none.public_key.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn edits_authorized_keys_without_touching_other_lines() {
        let dir = std::env::temp_dir().join(format!("klone-authz-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mk = |n: &str| {
            let k = dir.join(n);
            run(Command::new(SSH_KEYGEN).args(["-q", "-t", "ed25519", "-N", "", "-C"]).arg(format!("{n}@mac")).arg("-f").arg(&k)).unwrap();
            std::fs::read_to_string(pub_path(&k)).unwrap().trim().to_string()
        };
        let (a, b) = (mk("alice"), mk("bob"));
        let blob = |l: &str| parse_key_line(l).unwrap().1;

        let existing = format!("# managed by hand\nno-pty,from=\"10.0.0.1\" {a}\n");
        assert_eq!(parse_key_line(&format!("no-pty {a}")).unwrap().2, "alice@mac");

        // Pasted with stray whitespace → clean line; duplicates are refused.
        let clean = normalize_new_key(&format!("\n  {b}  \n")).unwrap();
        assert_eq!(clean, b);
        let added = add_authorized(&existing, &clean).unwrap();
        assert!(added.starts_with(&existing) && added.ends_with(&format!("{b}\n")));
        assert!(add_authorized(&added, &a).is_none(), "same key with different options is a duplicate");

        let listed = list_authorized(&added, Some(&blob(&a)));
        assert_eq!(listed.len(), 2);
        assert!(listed[0].mine && !listed[1].mine);
        assert!(listed[1].fingerprint.starts_with("SHA256:"));
        assert_eq!(listed[1].comment, "bob@mac");

        assert_eq!(remove_authorized(&added, &blob(&b)), existing);

        assert!(normalize_new_key("-----BEGIN OPENSSH PRIVATE KEY-----\nabc").unwrap_err().to_string().contains("private key"));
        assert!(normalize_new_key(&format!("{a}\n{b}")).is_err());
        assert!(normalize_new_key("hello world").is_err());
        assert!(normalize_new_key("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIbroken").is_err());

        // RFC 4716 export (e.g. from PuTTY) is converted.
        let rfc = run(Command::new(SSH_KEYGEN).args(["-e", "-m", "RFC4716", "-f"]).arg(pub_path(&dir.join("bob")))).unwrap();
        assert_eq!(blob(&normalize_new_key(&rfc).unwrap()), blob(&b));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Live: libssh2 must parse the keys `generate` makes. The box should
    /// answer "key not authorized", not fail locally on the key file.
    /// `cargo test libssh2_accepts -- --ignored`
    #[test]
    #[ignore]
    fn libssh2_accepts_generated_ed25519() {
        let dir = std::env::temp_dir().join(format!("klone-keyload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("k");
        run(Command::new(SSH_KEYGEN).args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key)).unwrap();
        let s = crate::config::live_test_box();
        let tcp = std::net::TcpStream::connect((s.host.as_str(), s.port)).unwrap();
        let mut sess = ssh2::Session::new().unwrap();
        sess.set_tcp_stream(tcp);
        sess.handshake().unwrap();
        let err = sess.userauth_pubkey_file(&s.user, None, &key, None).unwrap_err();
        // -18 AUTHENTICATION_FAILED / -19 PUBLICKEY_UNVERIFIED = server refused
        // a key libssh2 read fine. -16 FILE would mean a parsing problem.
        assert!(matches!(err.code(), ssh2::ErrorCode::Session(-18 | -19)), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
