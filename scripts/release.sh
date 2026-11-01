#!/bin/sh
# Release tape: scripts/release.sh 0.4.0
# Takes the list in UNRELEASED.md as the release notes, tests, bumps the version, builds the signed
# installer here, commits, tags and pushes, then publishes to the server's update folder, which Tailscale
# serves to the tailnet. Installed copies check that folder's latest.json. tape's repo is private,
# so GitHub Releases can't do this the way they do for kit and backdrop.
set -e
cd "$(dirname "$0")/.."
updates=/srv/tape-updates
base=https://updates.example.com:8448
v=${1:?usage: scripts/release.sh X.Y.Z}
notes=$(sed -n '/^- /,$p' UNRELEASED.md)
[ -n "$notes" ] || { echo "UNRELEASED.md has no changes listed"; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "commit or stash your changes first"; exit 1; }
git rev-parse -q --verify "refs/tags/v$v" >/dev/null && { echo "v$v already exists"; exit 1; }

cargo test -q -p tape-core -p tape-engine -p nvenc

sed -i "0,/^version = \".*\"/s//version = \"$v\"/" app/src-tauri/Cargo.toml crates/engine/Cargo.toml
sed -i "0,/\"version\": \".*\"/s//\"version\": \"$v\"/" app/src-tauri/tauri.conf.json
cargo update -p tape -p tape-engine --offline -q
sed -i '/^- /,$d' UNRELEASED.md

scripts/build-windows.sh
exe=tape_${v}_x64-setup.exe
sig=$(cat "dist/$exe.sig")

git add UNRELEASED.md Cargo.lock app/src-tauri/Cargo.toml crates/engine/Cargo.toml app/src-tauri/tauri.conf.json
git commit -qm "release $v"
printf '%s\n' "$notes" | git tag -a "v$v" -F -
git push -q origin main
git push -q origin "v$v"

# The installer goes first and latest.json last, so nothing ever points at a missing file.
cp "dist/$exe" "$updates/"
V="$v" NOTES="$notes" SIG="$sig" URL="$base/$exe" python3 - "$updates/latest.json.tmp" <<'PY'
import json, os, sys, datetime
json.dump({
    "version": os.environ["V"],
    "notes": os.environ["NOTES"],
    "pub_date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "platforms": {"windows-x86_64": {"signature": os.environ["SIG"], "url": os.environ["URL"]}},
}, open(sys.argv[1], "w"), indent=2)
PY
mv "$updates/latest.json.tmp" "$updates/latest.json"
echo "published $v"
printf '%s\n' "$notes"
