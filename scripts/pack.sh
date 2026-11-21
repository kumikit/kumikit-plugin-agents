#!/bin/sh
# Lists or packs the first-party plugins in kumikit-plugin-agents.
# Usage:
#   sh scripts/pack.sh --list
#   sh scripts/pack.sh <out-dir> [plugin-id ...]
set -eu

KUMIKIT_BIN=${KUMIKIT_BIN:-kumikit}
PLUGINS="providers/antigravity/plugin.toml providers/claude/plugin.toml providers/copilot/plugin.toml providers/cursor/plugin.toml providers/opencode/plugin.toml"

read_field() {
  python3 -c 'import sys, tomllib; print(tomllib.load(open(sys.argv[1], "rb"))[sys.argv[2]])' "$1" "$2"
}

if [ "${1:-}" = "--list" ]; then
  for m in $PLUGINS; do
    id=$(read_field "$m" id)
    ver=$(read_field "$m" version)
    short=${id#kumikit.}
    printf '%s %s %s-v%s\n' "$id" "$ver" "$short" "$ver"
  done
  exit 0
fi

OUT=${1:?usage: scripts/pack.sh <out-dir> [plugin-id ...]}
shift
WANT=" $* "

mkdir -p "$OUT"
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/kk-pack.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT

want_id() {
  [ "$WANT" = "  " ] && return 0
  case "$WANT" in
    *" $1 "*) return 0 ;;
    *) return 1 ;;
  esac
}

for name in antigravity copilot cursor opencode; do
  id=$(read_field "providers/$name/plugin.toml" id)
  ver=$(read_field "providers/$name/plugin.toml" version)
  want_id "$id" || continue
  d="$STAGE/$name"
  mkdir -p "$d"
  cp "providers/$name/plugin.toml" "providers/$name/README.md" "$d/"
  "$KUMIKIT_BIN" plugin pack "$d" -o "$OUT/$id-$ver.kkp"
done

id=$(read_field "providers/claude/plugin.toml" id)
ver=$(read_field "providers/claude/plugin.toml" version)
if want_id "$id"; then
  wasm="target/wasm32-wasip2/release/kumikit_provider_claude.wasm"
  if [ ! -f "$wasm" ]; then
    echo "missing $wasm — run: cargo build --target wasm32-wasip2 --release -p kumikit-provider-claude" >&2
    exit 1
  fi
  d="$STAGE/claude"
  mkdir -p "$d"
  cp providers/claude/plugin.toml providers/claude/README.md providers/claude/claude.schema.json "$d/"
  cp "$wasm" "$d/plugin.wasm"
  "$KUMIKIT_BIN" plugin pack "$d" -o "$OUT/$id-$ver.kkp"
fi
