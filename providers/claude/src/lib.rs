//! `kumikit.provider-claude`: runs Claude Code directly over its `stream-json` stdio
//! protocol (`docs/spec/05-providers.md`), and serves the `claude` provider backend so
//! manifest-only plugins can run Claude Code against Anthropic-compatible endpoints (see
//! `examples/deepseek`).
//!
//! Each session runs in its own WASM instance (ADR 0010). [`stream`] holds the protocol
//! translation free of I/O and is tested natively against recorded transcripts.

pub mod config;
pub mod stream;

use std::cell::RefCell;

use kumikit_provider::{
    Child, Decision, Error, ErrorCode, Event, Model, Plugin, Provider, ProviderRef, SessionConfig,
    SpawnRequest, UserInput, events, process, steering, take_line,
};

use crate::config::{Config, McpServerSpec};
use crate::stream::{Action, Attachment, Options, Session};

fn split_attachments(raw: &str) -> (String, Vec<Attachment>) {
    const MARKER: &str = "[kumikit-attachments:";
    let Some(idx) = raw.rfind(MARKER) else {
        return (raw.to_owned(), Vec::new());
    };
    let rest = &raw[idx + MARKER.len()..];
    let Some(end) = rest.find(']') else {
        return (raw.to_owned(), Vec::new());
    };
    let manifest_path = &rest[..end];
    let Ok(manifest_text) = std::fs::read_to_string(manifest_path) else {
        return (raw.to_owned(), Vec::new());
    };
    let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&manifest_text) else {
        return (raw.to_owned(), Vec::new());
    };
    let Some(items) = manifest["attachments"].as_array() else {
        return (raw.to_owned(), Vec::new());
    };
    let attachments: Vec<Attachment> = items
        .iter()
        .filter_map(|item| {
            Some(Attachment {
                name: item["name"].as_str()?.to_owned(),
                mime_type: item["mimeType"].as_str()?.to_owned(),
                host_path: item["hostPath"].as_str()?.to_owned(),
                data: item["data"].as_str().map(str::to_owned),
            })
        })
        .collect();
    let clean = raw[..idx].trim_end_matches('\n').to_owned();
    (clean, attachments)
}

struct Claude;

struct State {
    session: Session,
    child: Child,
    buf: Vec<u8>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn err(code: ErrorCode, message: impl Into<String>) -> Error {
    Error::new(code, message)
}

fn is_cancelled(e: &Error) -> bool {
    e.code == ErrorCode::Cancelled
}

fn wit_event(e: stream::Event) -> Event {
    use stream::Event as S;
    let json = |v: &serde_json::Value| v.to_string();
    match e {
        S::TextDelta { text } => Event::TextDelta(text),
        S::TextDone { text } => Event::TextDone(text),
        S::ReasoningDelta { text } => Event::ReasoningDelta(text),
        S::ToolStarted { call, name, input } => Event::ToolStarted(events::ToolStarted {
            call,
            name,
            input: json(&input),
        }),
        S::ToolOutput { call, chunk } => Event::ToolOutput(events::ToolOutput { call, chunk }),
        S::ToolFinished { call, ok, result } => Event::ToolFinished(events::ToolFinished {
            call,
            ok,
            result: result.as_ref().map(json),
        }),
        S::Usage {
            input_tokens,
            output_tokens,
            cost,
        } => Event::Usage(events::Usage {
            input_tokens,
            output_tokens,
            cost,
        }),
    }
}

enum Stop {
    Ready(String),
    TurnDone,
}

enum Input {
    Line(String),
    Steer(UserInput),
    Eof,
}

impl State {
    fn read_next(&mut self) -> Result<Input, Error> {
        loop {
            if let Some(line) = take_line(&mut self.buf, false) {
                return Ok(Input::Line(String::from_utf8_lossy(&line).into_owned()));
            }
            if self.session.is_prompting() {
                match steering::read_stdout(&self.child, 64 * 1024)? {
                    steering::Read::Stdout(chunk) => self.buf.extend_from_slice(&chunk),
                    steering::Read::Input(input) => return Ok(Input::Steer(input)),
                    steering::Read::Eof => {
                        return Ok(match take_line(&mut self.buf, true) {
                            Some(l) => Input::Line(String::from_utf8_lossy(&l).into_owned()),
                            None => Input::Eof,
                        });
                    }
                }
            } else {
                match self.child.read_stdout(64 * 1024)? {
                    Some(chunk) => self.buf.extend_from_slice(&chunk),
                    None => {
                        return Ok(match take_line(&mut self.buf, true) {
                            Some(l) => Input::Line(String::from_utf8_lossy(&l).into_owned()),
                            None => Input::Eof,
                        });
                    }
                }
            }
        }
    }

    fn run(&mut self, mut queue: Vec<Action>) -> Result<Stop, Error> {
        queue.reverse();
        let mut cancelled = false;
        loop {
            let Some(action) = queue.pop() else {
                match self.read_next() {
                    Ok(Input::Line(l)) => {
                        queue.extend(self.session.on_line(&l).into_iter().rev());
                    }
                    Ok(Input::Steer(input)) => {
                        queue.extend(self.session.steer(&input.text).into_iter().rev());
                    }
                    Ok(Input::Eof) => return Err(err(ErrorCode::Io, "Claude Code exited")),
                    Err(e) if is_cancelled(&e) => {
                        cancelled = true;
                        queue.extend(self.session.cancel().into_iter().rev());
                    }
                    Err(e) => return Err(e),
                }
                continue;
            };
            match action {
                Action::Write(mut line) => {
                    line.push('\n');
                    self.child.write_stdin(line.as_bytes())?;
                }
                Action::Emit(e) => match events::emit(&wit_event(e)) {
                    Ok(()) => {}
                    Err(_) if cancelled => {}
                    Err(e) if is_cancelled(&e) => {
                        cancelled = true;
                        queue.extend(self.session.cancel().into_iter().rev());
                    }
                    Err(e) => return Err(e),
                },
                Action::Approve {
                    request_id,
                    kind,
                    summary,
                    detail,
                } => {
                    let decision = if cancelled {
                        stream::Decision::Cancelled
                    } else {
                        match events::request_approval(&kind, &summary, Some(&detail.to_string())) {
                            Ok(Decision::Allow) => stream::Decision::Allow,
                            Ok(Decision::AllowAlways) => stream::Decision::AllowAlways,
                            Ok(Decision::Deny) => stream::Decision::Deny,
                            Err(e) if is_cancelled(&e) => {
                                cancelled = true;
                                queue.extend(self.session.cancel().into_iter().rev());
                                stream::Decision::Cancelled
                            }
                            Err(e) => return Err(e),
                        }
                    };
                    queue.extend(
                        self.session
                            .decide(&request_id, &detail, decision)
                            .into_iter()
                            .rev(),
                    );
                }
                Action::Ready { session } => return Ok(Stop::Ready(session)),
                Action::TurnDone => return Ok(Stop::TurnDone),
                Action::Failed(m) => return Err(err(ErrorCode::Internal, m)),
            }
        }
    }
}

fn config(provider: &ProviderRef) -> Result<Config, Error> {
    Config::from_option(provider.config.as_deref()).map_err(|e| err(ErrorCode::InvalidArgument, e))
}

impl Plugin for Claude {
    fn activate() -> Result<(), Error> {
        Ok(())
    }

    fn deactivate() {
        STATE.with(|s| {
            if let Some(state) = s.borrow_mut().take() {
                state.child.kill();
            }
        });
    }
}

/// Probes Claude Code's `initialize` control response to discover the exact versioned
/// models available to the local CLI. Returns `None` if `claude` cannot be started or
/// reports no models, so `list_models` falls back cleanly to static aliases.
fn probe_models(config: &Config) -> Option<Vec<Model>> {
    let mut env: Vec<(String, String)> = config
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if !env
        .iter()
        .any(|(k, _)| k == "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC")
    {
        env.push((
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".into(),
            "1".into(),
        ));
    }
    let child = process::spawn(&SpawnRequest {
        program: config.command.clone(),
        args: config.args_for("default", None, None),
        cwd: None,
        env,
        secret_env: config.secret_env.clone().into_iter().collect(),
    })
    .ok()?;
    let mut state = State {
        session: Session::new(Options::default()),
        child,
        buf: Vec::new(),
    };
    let start = state.session.start();
    let res = state.run(start);
    state.child.close_stdin();
    state.child.kill();
    let Ok(Stop::Ready(_)) = res else {
        return None;
    };
    let discovered = state.session.discovered_models();
    if discovered.is_empty() {
        return None;
    }
    let caps = vec![
        "tools".into(),
        "approvals".into(),
        "reasoning".into(),
        "resume".into(),
        "images".into(),
        "plan-mode".into(),
        "steer".into(),
    ];
    Some(
        discovered
            .iter()
            .map(|m| Model {
                id: m.id.clone(),
                label: m.label.clone(),
                capabilities: caps.clone(),
            })
            .collect(),
    )
}

impl Provider for Claude {
    fn list_models(provider: ProviderRef) -> Result<Vec<Model>, Error> {
        let cfg = config(&provider)?;
        if !cfg.models.is_empty() {
            return Ok(cfg
                .models()
                .into_iter()
                .map(|m| Model {
                    id: m.id,
                    label: m.label,
                    capabilities: m.capabilities,
                })
                .collect());
        }
        if let Some(models) = probe_models(&cfg) {
            return Ok(models);
        }
        Ok(cfg
            .models()
            .into_iter()
            .map(|m| Model {
                id: m.id,
                label: m.label,
                capabilities: m.capabilities,
            })
            .collect())
    }

    fn start_session(provider: ProviderRef, cfg: SessionConfig) -> Result<String, Error> {
        let config = config(&provider)?;
        let mcp = cfg.mcp_server.map(|m| McpServerSpec {
            name: m.name,
            command: m.command,
            args: m.args,
            env: m.env,
        });
        let permission_mode = cfg
            .options
            .iter()
            .find(|(k, _)| k == "permission-mode")
            .map(|(_, v)| v.as_str());
        let child = process::spawn(&SpawnRequest {
            program: config.command.clone(),
            args: config.args_for_with_mode(
                &cfg.model,
                cfg.resume.as_deref(),
                mcp.as_ref(),
                permission_mode,
            ),
            cwd: None,
            env: config.env.clone().into_iter().collect(),
            secret_env: config.secret_env.clone().into_iter().collect(),
        })?;
        let mut state = State {
            session: Session::new(Options { resume: cfg.resume }),
            child,
            buf: Vec::new(),
        };
        let start = state.session.start();
        let id = match state.run(start)? {
            Stop::Ready(id) => id,
            Stop::TurnDone => return Err(err(ErrorCode::Internal, "unexpected turn end")),
        };
        STATE.with(|s| *s.borrow_mut() = Some(state));
        Ok(id)
    }

    fn send(_session: String, input: UserInput) -> Result<(), Error> {
        STATE.with(|s| {
            let mut guard = s.borrow_mut();
            let state = guard
                .as_mut()
                .ok_or_else(|| err(ErrorCode::Conflict, "no session is open"))?;
            let (text, attachments) = split_attachments(&input.text);
            let prompt = state.session.prompt_with_attachments(&text, &attachments);
            state.run(prompt).map(|_| ())
        })
    }

    fn end_session(_session: String) {
        STATE.with(|s| {
            if let Some(state) = s.borrow_mut().take() {
                state.child.close_stdin();
                state.child.kill();
            }
        });
    }
}

kumikit_provider::export_provider!(Claude);
