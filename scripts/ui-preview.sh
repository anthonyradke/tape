#!/bin/sh
# Serves app/ui in a normal browser with the fake backend from ui-mock.js.
# Open http://127.0.0.1:8766/?scene=replay|recording|trouble|off|conflict  (&empty=1: no clips)
set -e
here=$(cd "$(dirname "$0")" && pwd)
dir=$(mktemp -d)
cp -r "$here/../app/ui/." "$dir/"
cp "$here/ui-mock.js" "$dir/mock.js"
sed -i 's#<script src="app.js"></script>#<script src="mock.js"></script><script src="app.js"></script>#' "$dir/index.html"
cd "$dir" && exec python3 -m http.server 8766 --bind 127.0.0.1
