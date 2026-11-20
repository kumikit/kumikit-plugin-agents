//! Replays recorded Claude Code `stream-json` output through `kumikit_provider_claude::stream::Session`
//! and compares the emitted events with `tests/expected/<name>.json` (`KUMIKIT_BLESS=1` rewrites them).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use kumikit_provider_claude::stream::{Action, Decision, Options, Session};
use serde_json::{Value, json};

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
                Action::Emit(e) => self.log.push(serde_json::to_value(e).unwrap()),
                Action::Approve {
                    request_id,
                    kind,
                    summary,
                    detail,
                } => {
                    self.log
                        .push(json!({"approve": {"kind": kind, "summary": summary}}));
                    self.approvals.insert(request_id, detail);
                }
                Action::Ready { session } => self.log.push(json!({"ready": session})),
                Action::TurnDone => self.log.push(json!("turn_done")),
                Action::Failed(m) => self.log.push(json!({"failed": m})),
            }
        }
    }
}

fn replay(path: &Path) -> Result<Vec<Value>, String> {
    let name = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|e| format!("{name}: {e}"))?;
    let lines: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{name}: {e}"))?;
    let mut r = Replay {
        session: Session::new(Options::default()),
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
        if entry["from"] != "client" {
            continue;
        }
        match msg["type"].as_str() {
            Some("user") => {
                let text = msg["message"]["content"].as_str().unwrap_or_default();
                let actions = r.session.prompt(text);
                r.take(actions);
            }
            Some("control_request") if msg["request"]["subtype"].as_str() == Some("interrupt") => {
                let actions = r.session.cancel();
                r.take(actions);
            }
            Some("control_response") => {
                let req_id = msg["response"]["request_id"]
                    .as_str()
                    .ok_or_else(|| format!("{name}[{i}]: missing request_id"))?;
                let detail =
                    r.approvals.get(req_id).cloned().ok_or_else(|| {
                        format!("{name}[{i}]: answer to unknown request {req_id}")
                    })?;
                let resp = &msg["response"]["response"];
                let decision = match resp["behavior"].as_str() {
                    Some("allow") if resp.get("updatedPermissions").is_some() => {
                        Decision::AllowAlways
                    }
                    Some("allow") => Decision::Allow,
                    _ => Decision::Deny,
                };
                let actions = r.session.decide(req_id, &detail, decision);
                r.take(actions);
            }
            _ => {}
        }
        let written = r
            .writes
            .pop_front()
            .ok_or_else(|| format!("{name}[{i}]: session wrote nothing; expected {msg}"))?;
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

#[test]
fn claude_recordings_replay_to_their_expected_events() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/transcripts");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no recordings in {}", dir.display());
    let mut problems = Vec::new();
    for path in files {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let log = match replay(&path) {
            Ok(l) => l,
            Err(e) => {
                problems.push(e);
                continue;
            }
        };
        let expected = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/expected")
            .join(format!("{stem}.json"));
        let actual = serde_json::to_string_pretty(&log).unwrap() + "\n";
        if std::env::var_os("KUMIKIT_BLESS").is_some() {
            std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
            std::fs::write(&expected, &actual).unwrap();
        } else if std::fs::read_to_string(&expected).ok().as_deref() != Some(actual.as_str()) {
            problems.push(format!(
                "{stem}: events differ from {} (KUMIKIT_BLESS=1 to accept)",
                expected.display()
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
