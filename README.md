# Klone

A menu bar app for macOS that archives large media files to a Hetzner Storage Box and retrieves them again.

- **Storage boxes** work like drives. Add as many as you like in Settings. Each has its own name, vault folder, SSH key and connection budget. When you archive, Klone asks which boxes to send to, and you can pick one or several.
- **Libraries**: local folders, such as `/Volumes/Media/Projects`, that mirror into `<vault folder>/<library name>/…` on each box.
- **Archive**: copies files to the vault and keeps the local copies.
- **Archive & free space**: uploads, checks the MD5 checksum on each storage box itself, and only once **every chosen box** has a verified copy of that exact version moves the local copy to the Trash (or deletes it, if you choose that in Settings).
- The library view shows a chip per box, lit when that box holds a copy.
- **Vault-only files** still appear in the library as italic "ghost" rows. Click ↓ to retrieve them.
- **Drag and drop** onto the window to archive anything. Files from outside a library go to `vault/Unsorted`.

## Reliability

| Concern | How Klone handles it |
| --- | --- |
| Interrupted transfers | Progress is recorded per byte range in `~/Library/Application Support/Klone/jobs.json`. A transfer resumes from the last confirmed byte after a network drop, sleep, crash, or restart. |
| Corrupt resume | Before resuming, the last 256 KB of each partial range is compared on both sides. If they differ, that range starts over. |
| Half-written files | Data goes to a hidden `.name.klone-part` file. It is renamed to the real name only after verification. |
| Silent corruption | Both sides compute an MD5 (the storage box runs `md5sum` itself). If they don't match, the file is transferred again. |
| Network outages | Klone retries with backoff (up to 60 s between attempts), reconnects, and resumes. A stalled connection is dropped after 45 s. |
| Mac sleeping mid-transfer | `caffeinate` keeps the Mac awake while anything is queued. |
| Storage box 10-connection limit | Each box has its own budget (default 8, maximum 10). One connection per box is reserved for browsing. If several Macs use the same box, lower each one's budget. |
| Portable drive unplugged | Transfers for it switch to **Waiting for drive** instead of failing, and continue on their own when the drive is mounted again. The library shows what's already in the vault. |

## Speed

Files larger than 256 MB are split into segments, and each segment travels over its own SSH connection. A single SSH stream is limited by its window size on high-latency links, so parallel connections are what make large transfers fast. You can tune file concurrency, streams per file, the split size, and a bandwidth cap in Settings. The lamps in the bottom bar show how many connections are in use.

## Install

Download the latest `Klone_x.y.z_universal.dmg` from [Releases](https://github.com/secedastudios/klone/releases), open it and drag Klone to Applications. It runs on Apple Silicon and Intel Macs (macOS 12+).

The app isn't notarized by Apple, so the first launch is blocked: right-click Klone → **Open**, or allow it under System Settings → Privacy & Security.

On first launch Klone opens Settings. Enter your storage box's server (e.g. `u123456.your-storagebox.de`) and username, set up an SSH key (below), and click **Test connection**.

### Updates

Klone checks GitHub for a new release shortly after launch and every few hours after that. When one is out, an **Update to x.y.z** button appears in the sidebar. Installing downloads the update, checks its signature against the key built into the app, and restarts. Transfers that were running pick up where they left off. Settings → About shows the version and has **Check for updates**.

## Build and run

```sh
make dev         # run from source
make install     # build, replace /Applications/Klone.app, relaunch
make universal   # universal .dmg (needs both Rust targets: rustup target add aarch64-apple-darwin x86_64-apple-darwin)
make bump        # bump the version (PART=minor|major, or V=1.2.3)
make test
```

### Releasing

```sh
make bump                       # new version number
git commit -am "…" && git push
make release                    # tags v<version> and pushes the tag
```

Pushing the tag starts the **Release** workflow (`.github/workflows/release.yml`). It runs the tests on a GitHub macOS runner, builds a universal app, signs the update archive, and publishes a GitHub release with the .dmg, the update archive and `latest.json`. Installed copies find it through `https://github.com/secedastudios/klone/releases/latest/download/latest.json`.

The workflow signs with the repository secret `TAURI_SIGNING_PRIVATE_KEY`, which holds the contents of `~/.tauri/klone.key`. `make release-local` does the same build and publish from your Mac, if CI is unavailable.

**Keep `~/.tauri/klone.key` safe and backed up.** Installed copies only accept updates signed with it, so if it's lost they can't be updated and everyone has to reinstall by hand. Never commit it. Without the key, the other `make` targets still build; they just skip the update archive.

### SSH key setup

Settings → **SSH key** shows the configured key with its fingerprint and public key.

- **Create a key** generates a passwordless ed25519 key at `~/.ssh/klone_ed25519`, switches Klone to it, and copies the public key to the clipboard. Existing keys are never overwritten.
- **Copy public key** puts the one-line public key on the clipboard.

Add that line to the storage box's `.ssh/authorized_keys`, then click **Test connection**.

### Giving teammates access

Once your own key works, Settings → your box → **Who can sign in** → **Manage access keys** lists every key that can sign in. Paste a teammate's public key (or pick their `.pub` file) to add it, or remove one. Klone keeps a backup of the old list on the box, and after each change it signs in again with its own key; if that fails, it puts the old list back. It won't remove the key it signs in with, or the last remaining key.

Your teammate adds a box in their own Klone with the same server and username, creates their own key, and sends you only the public half.

Klone tries the configured key first and falls back to ssh-agent. On a fresh install it picks the first key it finds among `~/.ssh/klone_ed25519`, `id_ed25519`, `id_ecdsa` and `id_rsa`. The storage box host key is checked against `~/.ssh/known_hosts`. If the host isn't listed yet, its key is added on first connect.

## Tests

```sh
cargo test                                                         # unit tests
# live tests against a real box (they create and remove their own test folders):
KLONE_TEST_HOST=u123456.your-storagebox.de KLONE_TEST_USER=u123456 [KLONE_TEST_KEY=~/.ssh/key] \
  KLONE_DATA_DIR=$(mktemp -d) cargo test -- --ignored --nocapture
```
