# Klone — build and install the macOS app.
#
#   make build     build Klone.app and the .dmg (this Mac's architecture)
#   make universal build a universal (Apple Silicon + Intel) .dmg
#   make install   build, quit the running Klone, replace /Applications/Klone.app, relaunch
#   make bump      bump the patch version (or: make bump PART=minor|major, make bump V=1.2.3)
#   make release   tag and push v<version>; GitHub Actions publishes it (installed apps auto-update)
#   make dev       run in development mode
#   make test      unit tests

APP_NAME    := Klone
BUNDLE_DIR  := target/release/bundle
UNI_DIR     := target/universal-apple-darwin/release/bundle
APP         := $(BUNDLE_DIR)/macos/$(APP_NAME).app
INSTALL_DIR ?= /Applications
VERSION     := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
PART        ?= patch

# Updater signing key. Without it, builds skip the update archive.
# Looked for in .secrets/ (gitignored) first, then ~/.tauri/.
UPDATER_KEY ?= $(firstword $(wildcard .secrets/klone.key $(HOME)/.tauri/klone.key) .secrets/klone.key)
ifneq ($(wildcard $(UPDATER_KEY)),)
export TAURI_SIGNING_PRIVATE_KEY := $(shell cat "$(UPDATER_KEY)")
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD ?=
TAURI_FLAGS :=
else
TAURI_FLAGS := --config '{"bundle":{"createUpdaterArtifacts":false}}'
endif
INSTALLED   := $(INSTALL_DIR)/$(APP_NAME).app

.PHONY: release release-local bump version build app dmg universal universal-app install uninstall dev test clean

version:
	@echo $(VERSION)

# Cargo.toml is the only place the version lives; tauri.conf.json inherits it.
bump:
	@new="$(V)"; if [ -z "$$new" ]; then \
		IFS=. read -r ma mi pa <<< "$(VERSION)"; \
		case "$(PART)" in major) new="$$((ma+1)).0.0";; minor) new="$$ma.$$((mi+1)).0";; *) new="$$ma.$$mi.$$((pa+1))";; esac; \
	fi; \
	sed -i '' "1,/^version = /s/^version = \".*\"/version = \"$$new\"/" Cargo.toml; \
	cargo update -p klone --offline -q 2>/dev/null || cargo metadata -q --format-version 1 >/dev/null; \
	echo "$(VERSION) -> $$new"

# Tag v<version> and push it; GitHub Actions builds and publishes.
release:
	scripts/release.sh tag

# Fallback: build, sign and publish from this Mac with ~/.tauri/klone.key.
release-local:
	UPDATER_KEY="$(UPDATER_KEY)" scripts/release.sh publish

build:
	cargo tauri build $(TAURI_FLAGS)

app:
	cargo tauri build $(TAURI_FLAGS) --bundles app

dmg: build
	@ls $(BUNDLE_DIR)/dmg/*.dmg

# Needs both Rust targets: rustup target add aarch64-apple-darwin x86_64-apple-darwin
universal:
	cargo tauri build $(TAURI_FLAGS) --target universal-apple-darwin
	@ls $(UNI_DIR)/dmg/*.dmg

universal-app:
	cargo tauri build $(TAURI_FLAGS) --target universal-apple-darwin --bundles app

install: app
	@if pgrep -f "$(INSTALLED)/Contents/MacOS/" >/dev/null; then \
		echo "Quitting running $(APP_NAME) (transfers resume after relaunch)…"; \
		osascript -e 'tell application "$(APP_NAME)" to quit' >/dev/null 2>&1 || true; \
		for i in 1 2 3 4 5 6 7 8 9 10; do \
			pgrep -f "$(INSTALLED)/Contents/MacOS/" >/dev/null || break; sleep 1; \
		done; \
		pkill -f "$(INSTALLED)/Contents/MacOS/" 2>/dev/null || true; \
		sleep 1; \
	fi
	rm -rf "$(INSTALLED)"
	ditto "$(APP)" "$(INSTALLED)"
	@echo "Installed $(INSTALLED)"
	open "$(INSTALLED)"

uninstall:
	-osascript -e 'tell application "$(APP_NAME)" to quit' >/dev/null 2>&1
	rm -rf "$(INSTALLED)"
	@echo "Removed $(INSTALLED) (settings and queue kept in ~/Library/Application Support/Klone)"

dev:
	cargo run

test:
	cargo test

clean:
	cargo clean
