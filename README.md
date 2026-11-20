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

## Tests

Recorded transcripts of each real agent replay through provider-acp's ACP translation:

```sh
cargo test -p kumikit-agents-harness       # KUMIKIT_BLESS=1 rewrites tests/expected
```

End to end, against a kumikit checkout at the pinned commit, built with
`cargo build -p kumikit-core -p kumikit-test-plugins`:

```sh
KUMIKIT_DIR=../kumikit cargo test -p kumikit-agents-e2e
```

Live smoke tests against the installed CLIs (ignored by default; they spend a turn on
your account):

```sh
KUMIKIT_DIR=../kumikit cargo test -p kumikit-agents-e2e -- --ignored
```

## Recording

```sh
KUMIKIT_DIR=../kumikit sh scripts/record.sh <provider> [scenario…]
```

`RECORD_EXTRA_ARGS` (a JSON array) and `RECORD_EXTRA_ENV` (a JSON object) add arguments or
variables for the recorded agent only, for example to pick a model a free plan has. The
scrubber removes session ids, paths, the user name and the gateway token, and refuses a
file that still has a personal detail in it.

## Pin

The SDK dependencies and CI pin one kumikit commit: `sh scripts/pin.sh <commit>`.
