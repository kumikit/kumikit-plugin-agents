//! Configuration for `kumikit.provider-claude` and delegated `claude` backend entries
//! (`claude.schema.json`).

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::json;

/// A model offered by a Claude Code provider entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    /// Model id or alias passed to `--model` (except `"default"`).
    pub id: String,
    /// Display label.
    pub label: String,
    /// Capability strings.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

/// A stdio MCP server to hand to Claude Code via `--mcp-config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSpec {
    /// Server name (e.g. `"kumikit"`).
    pub name: String,
    /// Absolute path of the program.
    pub command: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Environment variables (contains a per-session token: never logged).
    pub env: Vec<(String, String)>,
}

impl McpServerSpec {
    /// Serialises to the JSON object `--mcp-config` expects.
    pub fn to_mcp_config_json(&self) -> String {
        let env: serde_json::Map<String, serde_json::Value> = self
            .env
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        json!({
            "mcpServers": {
                &self.name: {
                    "type": "stdio",
                    "command": self.command,
                    "args": self.args,
                    "env": env,
                }
            }
        })
        .to_string()
    }
}

fn default_command() -> String {
    "claude".to_owned()
}

fn default_model_arg() -> Option<String> {
    Some("--model".to_owned())
}

fn default_permission_mode() -> String {
    "manual".to_owned()
}

/// How to start Claude Code (either the direct `claude` provider or a delegated entry).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// Program name (default `"claude"`).
    #[serde(default = "default_command")]
    pub command: String,
    /// Extra arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Plain environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Variables set from secrets: variable name -> secret name.
    #[serde(default)]
    pub secret_env: BTreeMap<String, String>,
    /// Models to offer.
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    /// Flag that selects the model on the command line (default `--model`).
    #[serde(default = "default_model_arg")]
    pub model_arg: Option<String>,
    /// Permission mode passed to `--permission-mode` (default `"manual"`).
    #[serde(default = "default_permission_mode")]
    pub permission_mode: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            command: default_command(),
            args: Vec::new(),
            env: BTreeMap::new(),
            secret_env: BTreeMap::new(),
            models: Vec::new(),
            model_arg: default_model_arg(),
            permission_mode: default_permission_mode(),
        }
    }
}

impl Config {
    /// Parses a delegated `claude` table (JSON) or returns the default direct config when
    /// `text` is `None`.
    pub fn from_option(text: Option<&str>) -> Result<Config, String> {
        match text {
            None => Ok(Config::default()),
            Some(s) => serde_json::from_str(s).map_err(|e| format!("invalid claude table: {e}")),
        }
    }

    /// Models offered by this provider.
    pub fn models(&self) -> Vec<ModelEntry> {
        if self.models.is_empty() {
            let caps = vec![
                "tools".into(),
                "approvals".into(),
                "reasoning".into(),
                "resume".into(),
                "images".into(),
                "plan-mode".into(),
                "steer".into(),
            ];
            vec![
                ModelEntry {
                    id: "default".into(),
                    label: "Default".into(),
                    capabilities: caps.clone(),
                },
                ModelEntry {
                    id: "sonnet".into(),
                    label: "Claude Sonnet".into(),
                    capabilities: caps.clone(),
                },
                ModelEntry {
                    id: "opus".into(),
                    label: "Claude Opus".into(),
                    capabilities: caps.clone(),
                },
                ModelEntry {
                    id: "haiku".into(),
                    label: "Claude Haiku".into(),
                    capabilities: caps,
                },
            ]
        } else {
            self.models.clone()
        }
    }

    /// Maps a Kumikit thread `permission-mode` option to Claude Code's `--permission-mode`.
    pub fn map_permission_mode(&self, option: Option<&str>) -> String {
        match option {
            Some("accept_edits") => "acceptEdits".to_owned(),
            Some("plan") => "plan".to_owned(),
            Some("full_access") => "bypassPermissions".to_owned(),
            _ => self.permission_mode.clone(),
        }
    }

    /// Command-line arguments for a Claude Code `stream-json` session.
    pub fn args_for(
        &self,
        model: &str,
        resume: Option<&str>,
        mcp: Option<&McpServerSpec>,
    ) -> Vec<String> {
        self.args_for_with_mode(model, resume, mcp, None)
    }

    /// Command-line arguments for a Claude Code `stream-json` session with an optional
    /// Kumikit permission mode (`standard`, `accept_edits`, `plan`, `full_access`).
    pub fn args_for_with_mode(
        &self,
        model: &str,
        resume: Option<&str>,
        mcp: Option<&McpServerSpec>,
        permission_mode: Option<&str>,
    ) -> Vec<String> {
        let mut out = self.args.clone();
        out.extend([
            "-p".to_owned(),
            "--verbose".to_owned(),
            "--input-format".to_owned(),
            "stream-json".to_owned(),
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--include-partial-messages".to_owned(),
            "--permission-prompt-tool".to_owned(),
            "stdio".to_owned(),
            "--permission-mode".to_owned(),
            self.map_permission_mode(permission_mode),
        ]);
        if let Some(flag) = &self.model_arg
            && !model.is_empty()
            && model != "default"
        {
            out.push(flag.clone());
            out.push(model.to_owned());
        }
        if let Some(id) = resume
            && !id.is_empty()
        {
            out.push("--resume".to_owned());
            out.push(id.to_owned());
        }
        if let Some(server) = mcp {
            out.push("--mcp-config".to_owned());
            out.push(server.to_mcp_config_json());
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn default_and_delegated_configs() {
        let def = Config::from_option(None).unwrap();
        assert_eq!(def.command, "claude");
        assert_eq!(def.models()[0].id, "default");
        let args = def.args_for("sonnet", Some("sess-1"), None);
        assert!(args.contains(&"--input-format".to_owned()));
        assert!(args.contains(&"stream-json".to_owned()));
        assert!(args.contains(&"--model".to_owned()));
        assert!(args.contains(&"sonnet".to_owned()));
        assert!(args.contains(&"--resume".to_owned()));
        assert!(args.contains(&"sess-1".to_owned()));

        let delegated = Config::from_option(Some(
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://api.deepseek.com/anthropic"},"secret-env":{"ANTHROPIC_AUTH_TOKEN":"deepseek-api-key"},"models":[{"id":"deepseek-chat","label":"DeepSeek Chat"}]}"#,
        ))
        .unwrap();
        assert_eq!(delegated.command, "claude");
        assert_eq!(
            delegated.env.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("https://api.deepseek.com/anthropic")
        );
        assert_eq!(
            delegated
                .secret_env
                .get("ANTHROPIC_AUTH_TOKEN")
                .map(String::as_str),
            Some("deepseek-api-key")
        );
        assert_eq!(delegated.models().len(), 1);
        assert!(Config::from_option(Some(r#"{"unknown":1}"#)).is_err());
    }

    #[test]
    fn mcp_config_json_has_stdio_server() {
        let spec = McpServerSpec {
            name: "kumikit".into(),
            command: "/kumikit".into(),
            args: vec!["mcp".into(), "--socket".into(), "/s".into()],
            env: vec![("KUMIKIT_MCP_TOKEN".into(), "tok".into())],
        };
        let v: serde_json::Value = serde_json::from_str(&spec.to_mcp_config_json()).unwrap();
        assert_eq!(v["mcpServers"]["kumikit"]["type"], "stdio");
        assert_eq!(v["mcpServers"]["kumikit"]["command"], "/kumikit");
        assert_eq!(
            v["mcpServers"]["kumikit"]["env"]["KUMIKIT_MCP_TOKEN"],
            "tok"
        );
    }

    #[test]
    fn maps_kumikit_permission_modes_to_claude_cli_flags() {
        let cfg = Config::default();
        assert_eq!(cfg.map_permission_mode(None), "manual");
        assert_eq!(cfg.map_permission_mode(Some("standard")), "manual");
        assert_eq!(cfg.map_permission_mode(Some("accept_edits")), "acceptEdits");
        assert_eq!(cfg.map_permission_mode(Some("plan")), "plan");
        assert_eq!(
            cfg.map_permission_mode(Some("full_access")),
            "bypassPermissions"
        );
        let args = cfg.args_for_with_mode("default", None, None, Some("full_access"));
        let idx = args.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(args[idx + 1], "bypassPermissions");
    }
}
