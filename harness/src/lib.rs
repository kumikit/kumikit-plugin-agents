//! Replays recorded agent output through provider-acp's ACP translation
//! (`kumikit_provider_acp::acp::Session`, the code the plugin runs) and returns the events
//! it emits, so tests can compare them with `expected/<name>.json`.
//!
//! A transcript is JSON lines `{"from": "client" | "agent", "message": …}`, recorded with
//! `fake-acp-agent record` and scrubbed by `scripts/scrub.mjs`. Every message the session
//! writes must equal the recorded client message.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use kumikit_provider_acp::acp::{Action, Decision, McpStdio, Options, Session};
use serde_json::{Value, json};

/// A provider directory: its manifest's ACP table and its recordings.
#[derive(Debug, Clone)]
pub struct Provider {
    /// The directory name (`copilot`).
    pub name: String,
    /// The directory.
    pub dir: PathBuf,
    /// `acp.mode` from the manifest.
    pub mode: Option<String>,
    /// `acp.auth-method` from the manifest.
    pub auth_method: Option<String>,
}

/// Every provider under `providers/`.
pub fn providers(root: &Path) -> Result<Vec<Provider>, String> {
    let mut out = Vec::new();
    let dir = root.join("providers");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("plugin.toml").is_file())
        .collect();
    entries.sort();
    for d in entries {
        let text = std::fs::read_to_string(d.join("plugin.toml")).map_err(|e| e.to_string())?;
        let manifest: toml::Value =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", d.display()))?;
        if manifest.get("runtime").is_some() {
            continue;
        }
        let acp = manifest
            .get("contributes")
            .and_then(|c| c.get("providers"))
            .and_then(|p| p.get(0))
            .and_then(|p| p.get("acp"));
        let mode = acp
            .and_then(|a| a.get("mode"))
            .and_then(|m| m.as_str())
            .map(str::to_owned);
        let auth_method = acp
            .and_then(|a| a.get("auth-method"))
            .and_then(|m| m.as_str())
            .map(str::to_owned);
        let name = d
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push(Provider {
            name,
            dir: d,
            mode,
            auth_method,
        });
    }
    Ok(out)
}

/// The transcripts of a provider, by name.
pub fn transcripts(p: &Provider) -> Vec<(String, PathBuf)> {
    let dir = p.dir.join("tests/transcripts");
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)
        .map(|r| {
            r.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|f| f.extension().is_some_and(|x| x == "jsonl"))
                .filter_map(|f| Some((f.file_stem()?.to_string_lossy().into_owned(), f)))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// The MCP server the recorded `session/new` (or `session/load`) passed, if any: replay
/// hands the session the same one, so its write matches.
fn recorded_mcp(lines: &[Value]) -> Option<McpStdio> {
    let msg = lines
        .iter()
        .map(|e| &e["message"])
        .find(|m| matches!(m["method"].as_str(), Some("session/new" | "session/load")))?;
    let s = msg["params"]["mcpServers"].get(0)?;
    Some(McpStdio {
        name: s["name"].as_str()?.to_owned(),
        command: s["command"].as_str()?.to_owned(),
        args: s["args"]
            .as_array()?
            .iter()
            .filter_map(|a| a.as_str().map(str::to_owned))
            .collect(),
        env: s["env"]
            .as_array()?
            .iter()
            .filter_map(|e| {
                Some((
                    e["name"].as_str()?.to_owned(),
                    e["value"].as_str()?.to_owned(),
                ))
            })
            .collect(),
    })
}

struct Replay {
    session: Session,
    writes: VecDeque<String>,
    approvals: HashMap<String, Value>,
    log: Vec<Value>,
}

impl Replay {
    fn take(&mut self, actions: Vec<Action>) {
        for a in actions {
            match a {
                Action::Write(line) => self.writes.push_back(line),
                Action::Emit(e) => self
                    .log
                    .push(serde_json::to_value(e).unwrap_or(Value::Null)),
                Action::Approve {
                    request,
                    kind,
                    summary,
                    detail,
                } => {
                    self.log
                        .push(json!({"approve": {"kind": kind, "summary": summary}}));
                    self.approvals
                        .insert(request.to_string(), detail["options"].clone());
                }
                Action::Ready { session } => self.log.push(json!({"ready": session})),
                Action::TurnDone => self.log.push(json!("turn_done")),
                Action::Failed(m) => self.log.push(json!({"failed": m})),
            }
        }
    }
}

/// Replays one transcript and returns the events the session emitted.
pub fn replay(
    path: &Path,
    mode: Option<&str>,
    auth_method: Option<&str>,
) -> Result<Vec<Value>, String> {
    let name = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|e| format!("{name}: {e}"))?;
    let lines: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{name}: {e}"))?;
    let mut r = Replay {
        session: Session::new(Options {
            cwd: "/workspace".into(),
            resume: None,
            mode: mode.map(str::to_owned),
            auth_method: auth_method.map(str::to_owned),
            mcp: recorded_mcp(&lines),
            ..Options::default()
        }),
        writes: VecDeque::new(),
        approvals: HashMap::new(),
        log: Vec::new(),
    };
    let start = r.session.start();
    r.take(start);
    for (i, entry) in lines.iter().enumerate() {
        let msg = &entry["message"];
        if entry["from"] == "agent" {
            let actions = r.session.on_line(&msg.to_string());
            r.take(actions);
            continue;
        }
        match msg["method"].as_str() {
            Some("session/prompt") => {
                let text = msg["params"]["prompt"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let actions = r.session.prompt(&text);
                r.take(actions);
            }
            Some("session/cancel") => {
                let actions = r.session.cancel();
                r.take(actions);
            }
            Some(_) => {}
            None => {
                // An answer to a permission request: decide as the recorded user did.
                let options = r
                    .approvals
                    .get(&msg["id"].to_string())
                    .cloned()
                    .ok_or_else(|| format!("{name}[{i}]: an answer to no request"))?;
                let chosen = msg["result"]["outcome"]["optionId"]
                    .as_str()
                    .unwrap_or_default();
                let kind = options
                    .as_array()
                    .and_then(|o| o.iter().find(|o| o["optionId"] == chosen))
                    .and_then(|o| o["kind"].as_str())
                    .unwrap_or("reject_once")
                    .to_owned();
                let decision = match kind.as_str() {
                    "allow_once" => Decision::Allow,
                    "allow_always" => Decision::AllowAlways,
                    _ => Decision::Deny,
                };
                let actions = r.session.decide(&msg["id"], &options, decision);
                r.take(actions);
            }
        }
        let written = r
            .writes
            .pop_front()
            .ok_or_else(|| format!("{name}[{i}]: the session wrote nothing; expected {msg}"))?;
        let written: Value = serde_json::from_str(&written).map_err(|e| e.to_string())?;
        if &written != msg {
            return Err(format!("{name}[{i}]: wrote {written}\nrecorded {msg}"));
        }
    }
    if !r.writes.is_empty() {
        return Err(format!("{name}: unexpected writes {:?}", r.writes));
    }
    Ok(r.log)
}
