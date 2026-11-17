#!/usr/bin/env bash
# Klone releases.
#
#   scripts/release.sh tag       (make release)  check the tree is clean and
#                                pushed, then tag v<version> and push the tag;
#                                GitHub Actions builds and publishes it.
#   scripts/release.sh publish   build a signed universal app and publish the
#                                GitHub release for the current version. Run
#                                by CI; `make release-local` runs it here as a
#                                fallback, using .secrets/klone.key (or ~/.tauri/klone.key).
#
# NOTES="…" overrides the release notes (default: commit subjects since the
# previous tag).
set -euo pipefail
cd "$(dirname "$0")/.."

REPO="${REPO:-secedastudios/klone}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
TAG="v$VERSION"
die() { echo "release: $*" >&2; exit 1; }

released() { gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; }

tag() {
  [ -z "$(git status --porcelain)" ] || die "commit your changes first"
  git fetch -q --tags origin
  [ -n "$(git branch -r --contains HEAD)" ] || die "push first: HEAD isn't on GitHub yet"
  if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    die "$TAG already exists — run make bump for a new version"
  fi
  git tag -a "$TAG" -m "Klone $VERSION"
  git push -q origin "$TAG"
  echo "Pushed $TAG. GitHub Actions is building the release:"
  echo "  https://github.com/$REPO/actions"
}

publish() {
  if [ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]; then
    KEY="${UPDATER_KEY:-}"
    [ -n "$KEY" ] || for k in .secrets/klone.key "$HOME/.tauri/klone.key"; do [ -f "$k" ] && KEY="$k" && break; done
    [ -f "$KEY" ] || die "no updater signing key (TAURI_SIGNING_PRIVATE_KEY or $KEY)"
    export TAURI_SIGNING_PRIVATE_KEY="$(cat "$KEY")"
  fi
  export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}"
  ! released || die "$TAG is already released — run make bump"
  if [ -n "${GITHUB_REF_NAME:-}" ] && [ "$GITHUB_REF_NAME" != "$TAG" ]; then
    die "tag $GITHUB_REF_NAME doesn't match Cargo.toml version $VERSION"
  fi

  cargo tauri build --target universal-apple-darwin

  B=target/universal-apple-darwin/release/bundle
  OUT="dist/$TAG"
  rm -rf "$OUT" && mkdir -p "$OUT"
  cp "$B/dmg/Klone_${VERSION}_universal.dmg" "$OUT/"
  cp "$B/macos/Klone.app.tar.gz" "$OUT/Klone_universal.app.tar.gz"
  cp "$B/macos/Klone.app.tar.gz.sig" "$OUT/Klone_universal.app.tar.gz.sig"

  if [ -z "${NOTES:-}" ]; then
    PREV="$(git describe --tags --abbrev=0 "$TAG^" 2>/dev/null || git describe --tags --abbrev=0 HEAD^ 2>/dev/null || true)"
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

  # Tag pushed by `make release` (CI), or created here at HEAD (local fallback).
  local target=()
  git rev-parse -q --verify "refs/tags/$TAG" >/dev/null || target=(--target "$(git rev-parse HEAD)")
  gh release create "$TAG" --repo "$REPO" ${target[@]+"${target[@]}"} \
    --title "Klone $VERSION" --notes "$NOTES" "$OUT"/*
  echo "Released $TAG: https://github.com/$REPO/releases/tag/$TAG"
}

case "${1:-}" in
  tag) tag ;;
  publish) publish ;;
  *) die "usage: $0 tag|publish" ;;
esac
