# kumikit-plugin-agents

First-party agent provider plugins for [Kumikit](https://github.com/kumikit/kumikit).

## Declarative ACP providers

Manifests with no code, run by Kumikit's preinstalled `kumikit.provider-acp`:

| Plugin | Agent | Command |
|---|---|---|
| `kumikit.provider-antigravity` | Antigravity CLI | `agy acp` |
| `kumikit.provider-copilot` | GitHub Copilot CLI | `copilot --acp` |
| `kumikit.provider-cursor` | Cursor CLI | `cursor-agent acp` (ACP `authenticate` with `cursor_login`) |
| `kumikit.provider-opencode` | OpenCode | `opencode acp` |

Each uses the CLI's own sign-in and model settings. Each directory's README says how to
install and sign in.

## WASM provider adapters

| Plugin | Agent | Protocol |
|---|---|---|
| `kumikit.provider-claude` | Claude Code | Native `stream-json` stdio (`--permission-prompt-tool stdio`, `--mcp-config`); also serves the `claude` backend |

`providers/claude/examples/deepseek` is a community example using the `claude` backend to
run Claude Code against DeepSeek's Anthropic-compatible API.

## Tests

Unit and replay tests:

```sh
cargo test -p kumikit-agents-harness       # KUMIKIT_BLESS=1 rewrites tests/expected
cargo test -p kumikit-provider-claude
```

End to end, against a kumikit checkout at the pinned commit (`cargo build -p kumikit-core
-p kumikit-test-plugins`) and the built WASM adapter (`cargo build --target wasm32-wasip2
--release -p kumikit-provider-claude`):

```sh
KUMIKIT_DIR=../kumikit cargo test -p kumikit-agents-e2e
```

Live smoke tests against the installed CLIs (ignored by default; they spend a turn on
your account):

```sh
KUMIKIT_DIR=../kumikit cargo test -p kumikit-agents-e2e -- --ignored
```

## Recording

ACP providers:

```sh
KUMIKIT_DIR=../kumikit sh scripts/record.sh <provider> [scenario…]
```

Claude Code `stream-json` (drives the real `claude` binary against a loopback Anthropic
Messages API server):

```sh
KUMIKIT_DIR=../kumikit node scripts/record-claude.mjs
```

## Pin

The SDK dependencies and CI pin one kumikit commit: `sh scripts/pin.sh <commit>`.
