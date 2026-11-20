#!/bin/sh
# Records a provider's real agent: a throwaway core runs a copy of its manifest whose
# command goes through `fake-acp-agent record`, and scripts/drive.mjs plays the scenarios
# (scripts/scenarios.json). The raw recordings are scrubbed into
# providers/<name>/tests/transcripts/. Uses the agent's own sign-in; spends a few turns.
#
# Usage: KUMIKIT_DIR=<kumikit checkout> sh scripts/record.sh <provider> [scenario…]
# The checkout must be built: cargo build -p kumikit-core -p kumikit-test-plugins
set -eu
name=$1; shift
here=$(cd "$(dirname "$0")/.." && pwd)
k=${KUMIKIT_DIR:?set KUMIKIT_DIR to a built kumikit checkout}
kumikit="$k/target/debug/kumikit"
bin="$k/target/kumikit-test/release"
wasm="$k/target/kumikit-test/wasm32-wasip2/release"
[ -x "$kumikit" ] && [ -x "$bin/fake-acp-agent" ] || { echo "build $k first (see the usage line)" >&2; exit 1; }

tmp=$(mktemp -d /tmp/kkrec-XXXXXX); chmod 700 "$tmp"
core=
trap '[ -n "$core" ] && kill $core 2>/dev/null || true; rm -rf "$tmp"' EXIT
mkdir -p "$tmp/ws" "$tmp/raw" "$tmp/acp" "$tmp/tool/tools"
printf '# Demo\n\nA small project used to record agent transcripts.\n' > "$tmp/ws/README.md"

# provider-acp and the gateway's test tools, packed from the checkout.
cp "$k/plugins/provider-acp/plugin.toml" "$k/plugins/provider-acp/acp.schema.json" "$tmp/acp/"
cp "$wasm/kumikit_provider_acp.wasm" "$tmp/acp/plugin.wasm"
cp "$k/test-plugins/tool/plugin.toml" "$tmp/tool/"; cp "$k"/test-plugins/tool/tools/*.json "$tmp/tool/tools/"
cp "$wasm/kumikit_test_tool.wasm" "$tmp/tool/plugin.wasm"

# The recorder: one provider per scenario, each writing its own raw file.
node "$here/scripts/recorder-manifest.mjs" "$here/providers/$name/plugin.toml" "$tmp/raw" "$here/scripts/scenarios.json" > "$tmp/recorder.toml"
mkdir -p "$tmp/recorder"; mv "$tmp/recorder.toml" "$tmp/recorder/plugin.toml"

PATH="$bin:$PATH" "$kumikit" serve --socket "$tmp/s.sock" --data-dir "$tmp/data" > "$tmp/ready" 2> "$tmp/core.log" &
core=$!
while ! grep -q '^ready' "$tmp/ready" 2>/dev/null; do
  kill -0 $core 2>/dev/null || { cat "$tmp/core.log" >&2; exit 1; }
  sleep 0.1
done
node "$here/scripts/drive.mjs" "$tmp/s.sock" "$tmp/ws" "$tmp/acp" "$tmp/tool" "$tmp/recorder" "$here/scripts/scenarios.json" "$@"
kill $core; wait $core 2>/dev/null || true; core=
node "$here/scripts/scrub.mjs" "$tmp/raw" "$here/providers/$name/tests/transcripts" "$name" "$tmp/ws"
echo "recorded into providers/$name/tests/transcripts; now: KUMIKIT_BLESS=1 cargo test"
