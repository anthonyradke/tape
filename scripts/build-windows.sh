#!/usr/bin/env bash
# Cross-compiles the Windows app and installer on Linux with cargo-xwin, then copies the
# installer and its update signature to dist/. Needs: rustup target x86_64-pc-windows-msvc,
# cargo-xwin, tauri-cli, nsis, lld, and the signing key in ~/.tauri.
set -euo pipefail
cd "$(dirname "$0")/.."
source "$HOME/.cargo/env"
export XWIN_ACCEPT_LICENSE=1
TAURI_SIGNING_PRIVATE_KEY=$(cat "$HOME/.tauri/tape.key")
TAURI_SIGNING_PRIVATE_KEY_PASSWORD=$(cat "$HOME/.tauri/tape.key.password")
export TAURI_SIGNING_PRIVATE_KEY TAURI_SIGNING_PRIVATE_KEY_PASSWORD
(cd app/src-tauri && cargo tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc)
mkdir -p dist
cp target/x86_64-pc-windows-msvc/release/bundle/nsis/*-setup.exe* dist/
ls -la dist
