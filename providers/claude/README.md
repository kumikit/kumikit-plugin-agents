# Claude Code (`kumikit.provider-claude`)

WASM provider adapter for [Claude Code](https://docs.anthropic.com/en/docs/claude-code)
over its native `stream-json` stdio protocol — no ACP wrapper required.

It also exposes the `claude` provider backend so manifest-only plugins can run Claude
Code against any Anthropic-compatible API endpoint (see `examples/deepseek/`).

## Prerequisites

Install Claude Code and sign in:

```sh
claude --version
```

## Icon

`icon.svg` uses Anthropic's Claude mark (from `lobehub/lobe-icons`, `claude-color.svg`)
to identify the Claude Code harness. The mark remains the trademark of Anthropic, PBC.
