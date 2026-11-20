# Cursor CLI (`kumikit.provider-cursor`)

Declarative ACP provider for [Cursor CLI](https://cursor.com/cli), using
`kumikit.provider-acp` (`cursor-agent acp` with ACP `auth-method = "cursor_login"`).

## Prerequisites

Install Cursor CLI and sign in:

```sh
cursor-agent login
```

Verify it is on your `PATH`:

```sh
cursor-agent status
```
