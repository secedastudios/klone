#!/usr/bin/env bash
# Build a signed universal Klone and publish it as a GitHub release that
# installed copies pick up through the in-app updater.
#
#   make release                     # notes = commit subjects since the last release
#   make release NOTES="What's new"  # custom notes
#
# Needs: gh (logged in), the updater key at ~/.tauri/klone.key, a clean
# git tree whose HEAD is pushed, and a version not released yet (make bump).
set -euo pipefail
cd "$(dirname "$0")/.."

REPO="${REPO:-secedastudios/klone}"
KEY="${UPDATER_KEY:-$HOME/.tauri/klone.key}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
TAG="v$VERSION"
die() { echo "release: $*" >&2; exit 1; }

[ -f "$KEY" ] || die "no updater signing key at $KEY (cargo tauri signer generate -w $KEY)"
gh auth status >/dev/null 2>&1 || die "gh isn't logged in (gh auth login)"
[ -z "$(git status --porcelain)" ] || die "commit your changes first"
git fetch -q origin
[ -n "$(git branch -r --contains HEAD)" ] || die "push first: HEAD isn't on GitHub yet"
! gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1 || die "$TAG is already released — run make bump"

export TAURI_SIGNING_PRIVATE_KEY="$(cat "$KEY")"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}"
cargo tauri build --target universal-apple-darwin

B=target/universal-apple-darwin/release/bundle
OUT="dist/$TAG"
rm -rf "$OUT" && mkdir -p "$OUT"
cp "$B/dmg/Klone_${VERSION}_universal.dmg" "$OUT/"
cp "$B/macos/Klone.app.tar.gz" "$OUT/Klone_universal.app.tar.gz"
cp "$B/macos/Klone.app.tar.gz.sig" "$OUT/Klone_universal.app.tar.gz.sig"

if [ -z "${NOTES:-}" ]; then
  PREV="$(git describe --tags --abbrev=0 2>/dev/null || true)"
  NOTES="$(git log --no-merges --pretty='- %s' ${PREV:+$PREV..}HEAD | head -40)"
fi

# One universal build serves both Mac architectures.
URL="https://github.com/$REPO/releases/download/$TAG/Klone_universal.app.tar.gz"
VERSION="$VERSION" NOTES="$NOTES" URL="$URL" SIG="$(cat "$OUT/Klone_universal.app.tar.gz.sig")" \
python3 -I -c '
import json, os, datetime
p = {"signature": os.environ["SIG"], "url": os.environ["URL"]}
print(json.dumps({
  "version": os.environ["VERSION"],
  "notes": os.environ["NOTES"],
  "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
  "platforms": {"darwin-aarch64": p, "darwin-x86_64": p},
}, indent=2))' > "$OUT/latest.json"

gh release create "$TAG" --repo "$REPO" --target "$(git rev-parse HEAD)" \
  --title "Klone $VERSION" --notes "$NOTES" "$OUT"/*
echo "Released $TAG: https://github.com/$REPO/releases/tag/$TAG"
