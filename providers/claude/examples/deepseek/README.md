# DeepSeek (`community.provider-deepseek`)

Community example of a manifest-only provider using `kumikit.provider-claude`'s `claude`
backend to run Claude Code against DeepSeek's Anthropic-compatible API.

## Setup

1. Install and enable `kumikit.provider-claude`.
2. Pack and install this directory (`kumikit plugin add providers/claude/examples/deepseek`).
3. Set your DeepSeek API key on the plugin's Settings page in Kumikit (or with `secret.set`
   under `deepseek-api-key`).
