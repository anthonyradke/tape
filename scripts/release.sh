#!/bin/sh
# Release tape: scripts/release.sh 0.4.0
# Takes the list in UNRELEASED.md as the release notes (the tag message, which CI publishes with the
# installer and the in-app update shows), bumps the version, empties the list, commits, tags and
# pushes. CI then builds and publishes the release.
# Everything that can stop it is checked before a file changes, and a refused push takes the
# commit and tag back, so a failed run leaves the checkout as it was.
set -e
cd "$(dirname "$0")/.."
fail() {
  echo "$*" >&2
  exit 1
}

v=${1:?usage: scripts/release.sh X.Y.Z}
printf '%s\n' "$v" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || fail "the version is X.Y.Z, without a v: got $v"
notes=$(sed -n '/^- /,$p' UNRELEASED.md)
[ -n "$notes" ] || fail "UNRELEASED.md has no changes listed"

# Only main, clean and level with GitHub: anything else would tag a commit main doesn't have, or
# push a main that leaves the release behind.
[ "$(git symbolic-ref -q --short HEAD)" = main ] || fail "releases are made from main, not $(git symbolic-ref -q --short HEAD || echo 'a detached HEAD')"
[ -z "$(git status --porcelain)" ] || fail "commit or stash your changes first"
git fetch -q origin main
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || fail "main isn't the same as origin/main: pull or push first"
git rev-parse -q --verify "refs/tags/v$v" >/dev/null && fail "v$v already exists"
[ -z "$(git ls-remote --tags origin "refs/tags/v$v")" ] || fail "v$v already exists on GitHub"
printf '%s\n' "$notes" | grep -iq 'co-authored-by' && fail "UNRELEASED.md has a co-author line"

cargo test -q -p tape-core -p tape-engine -p nvenc || fail "tests failed, nothing was changed"
# CI tests on Windows and only publishes when that passes, so catch what would stop it here, before
# a tag exists: a commit that already failed on GitHub, or code that doesn't compile for Windows.
[ "$(gh run list --workflow ci.yml --commit "$(git rev-parse HEAD)" --json conclusion -q '.[0].conclusion' 2>/dev/null)" != failure ] ||
  fail "CI failed on this commit on GitHub; fix it first"
XWIN_ACCEPT_LICENSE=1 cargo xwin check -q --target x86_64-pc-windows-msvc --workspace --all-targets ||
  fail "tape doesn't build for Windows, nothing was changed"

# perl rather than sed -i, whose options differ between GNU and macOS. No /g: only the package's own
# version, the first one in each file.
perl -0pi -e "s/^version = \"[^\"]*\"/version = \"$v\"/m" app/src-tauri/Cargo.toml crates/engine/Cargo.toml
perl -0pi -e "s/\"version\": \"[^\"]*\"/\"version\": \"$v\"/" app/src-tauri/tauri.conf.json
cargo update -p tape -p tape-engine --offline -q
perl -0pi -e 's/^- .*\z//ms' UNRELEASED.md

start=$(git rev-parse HEAD)
git add UNRELEASED.md Cargo.lock app/src-tauri/Cargo.toml crates/engine/Cargo.toml app/src-tauri/tauri.conf.json
git commit -qm "release $v"
printf '%s\n' "$notes" | git tag -a "v$v" -F -

undo() {
  git tag -d "v$v" >/dev/null
  git reset -q --hard "$start"
  fail "$1 The release commit and tag were undone; nothing was pushed."
}
# The project's rules for what reaches GitHub.
[ -z "$(git log origin/main..HEAD --format=%B | grep -i 'co-authored-by')" ] || undo "A commit message has a co-author line."
git log origin/main..HEAD --format='%ae%n%ce' | grep -qv '@users\.noreply\.github\.com$' && undo "A commit isn't authored with the GitHub noreply address."

# Both or neither, so a refused push can't leave a tag on GitHub for a commit main doesn't have.
git push -q --atomic origin main "v$v" || undo "The push was refused."
echo "pushed v$v; CI is building the release:"
printf '%s\n' "$notes"
