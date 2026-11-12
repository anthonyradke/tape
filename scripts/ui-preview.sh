#!/bin/sh
# Serves app/ui in a normal browser with the fake backend from ui-mock.js.
# Open http://127.0.0.1:8766/?scene=replay|recording|trouble|off|conflict  (&empty=1: no clips, &quality=1: recent dropped frames)
# or /overlay.html with the same options. Put a video at app/ui's side as SAMPLE=path to fill the
# gallery. It has to be a format the browser decodes.
set -e
here=$(cd "$(dirname "$0")" && pwd)
dir=$(mktemp -d)
cp -r "$here/../app/ui/." "$dir/"
cp "$here/ui-mock.js" "$dir/mock.js"
sed -i 's#<script src="shared.js"></script>#<script src="mock.js"></script><script src="shared.js"></script>#' "$dir/index.html" "$dir/overlay.html"
[ -z "${SAMPLE:-}" ] || cp "$SAMPLE" "$dir/sample.mp4"
cd "$dir" && exec python3 -m http.server 8766 --bind 127.0.0.1
