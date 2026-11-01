#!/bin/sh
# Regenerates crates/nvenc/src/bindings.rs from NVIDIA's header.
# Needs bindgen-cli, libclang and rustfmt. Usage: scripts/gen-nvenc.sh [tag]
set -e
tag=${1:-n12.0.16.1}
root=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
git clone -q --depth 1 --branch "$tag" https://github.com/FFmpeg/nv-codec-headers.git "$tmp/h"
bindgen "$tmp/h/include/ffnvcodec/nvEncodeAPI.h" --no-doc-comments --no-prepend-enum-name \
  --default-enum-style consts --allowlist-type 'NV_ENC.*|NVENC.*|PNV.*|NV_ENCODE.*|_NV.*|GUID|RECT' \
  --allowlist-var 'NV_ENC.*|NVENC.*' --allowlist-function 'NvEnc.*' --with-derive-default --no-derive-debug \
  --raw-line '#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, clippy::all)]' \
  -o "$root/crates/nvenc/src/bindings.rs"
