//! Pure, I/O-free Claude Code `stream-json` stdio client.
//!
//! [`Session`] accepts lines from Claude Code ([`Session::on_line`]) and calls from the
//! plugin ([`Session::start`], [`Session::prompt`], [`Session::cancel`],
//! [`Session::decide`]) and returns [`Action`]s: lines to write, events to emit, approvals
//! to ask for, and lifecycle markers. The WASM glue in `lib.rs` performs the actions;
//! replay tests drive [`Session`] natively against recorded transcripts.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

/// An event for the running turn, in the kernel's vocabulary.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// `text.delta`
    TextDelta {
        /// Text chunk.
        text: String,
    },
    /// `text.done`
    TextDone {
        /// Accumulated assistant text for the turn.
        text: String,
    },
    /// `reasoning.delta`
    ReasoningDelta {
        /// Thinking chunk.
        text: String,
    },
    /// `tool.started`
    ToolStarted {
        /// Tool use id.
        call: String,
        /// Tool name.
        name: String,
        /// Tool input object.
        input: Value,
    },
    /// `tool.output`
    ToolOutput {
        /// Tool use id.
        call: String,
        /// Output text.
        chunk: String,
    },
    /// `tool.finished`
    ToolFinished {
        /// Tool use id.
        call: String,
        /// Whether it succeeded.
        ok: bool,
        /// Structured or string result, if any.
        result: Option<Value>,
    },
    /// `usage`
    Usage {
        /// Input tokens.
        input_tokens: u64,
        /// Output tokens.
        output_tokens: u64,
        /// Reported USD cost, if any.
        cost: Option<f64>,
    },
    /// `commands.available`
    CommandsAvailable {
        /// Slash commands advertised by Claude Code (`system/init` or `initialize`).
        commands: Vec<SlashCommand>,
    },
    /// `limit.reached`
    LimitReached {
        /// ISO-8601 UTC timestamp when the usage limit resets, if known.
        reset_at: Option<String>,
        /// Human-readable explanation.
        message: String,
    },
}

/// One slash command reported by Claude Code (`system/init` `slash_commands` or
/// `initialize` `commands`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SlashCommand {
    /// Command name without a leading `/`.
    pub name: String,
    /// Optional short description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional argument hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// The user's answer to a `can_use_tool` permission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Allow this call once.
    Allow,
    /// Allow and apply Claude Code's suggested permission rule updates.
    AllowAlways,
    /// Deny this call.
    Deny,
    /// Turn was cancelled before an answer arrived.
    Cancelled,
}

/// Something the host glue must perform.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Write this JSON line (without trailing newline) to Claude Code's stdin.
    Write(String),
    /// Emit a turn event.
    Emit(Event),
    /// Ask the user, then call [`Session::decide`].
    Approve {
        /// Claude Code's `control_request.request_id`.
        request_id: String,
        /// Approval kind (`"tool"`).
        kind: String,
        /// One-line summary.
        summary: String,
        /// Raw `control_request.request` payload (includes `input` and
        /// `permission_suggestions`).
        detail: Value,
    },
    /// `start` succeeded; session is ready for prompts.
    Ready {
        /// Claude Code's session id.
        session: String,
    },
    /// The current prompt finished.
    TurnDone,
    /// The current phase failed.
    Failed(String),
}

/// A model reported by Claude Code in its `initialize` control response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredModel {
    /// Model id or alias (`value`) to pass to `--model`.
    pub id: String,
    /// Human-readable display label (including version when reported).
    pub label: String,
}

/// A file or image attachment provided alongside a user prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// Display filename.
    pub name: String,
    /// MIME type.
    pub mime_type: String,
    /// Host path on disk where the file was materialized.
    pub host_path: String,
    /// Base64-encoded bytes (present for images).
    pub data: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Initializing,
    Ready,
    Prompting,
}

/// Session options.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Resumed session id, if any.
    pub resume: Option<String>,
}

/// One Claude Code `stream-json` session.
#[derive(Debug)]
pub struct Session {
    phase: Phase,
    next_req: u64,
    init_req_id: Option<String>,
    session_id: Option<String>,
    text: String,
    /// Whether `stream_event` delivered text deltas for the current assistant message.
    streamed_text: bool,
    /// Tool use ids already announced via `ToolStarted`.
    started_tools: HashSet<String>,
    /// Tool use ids already finished via `ToolFinished`.
    finished_tools: HashSet<String>,
    /// Order in which tools started, so unfinished ones can be closed deterministically.
    tool_order: Vec<String>,
    /// Input map remembered for `can_use_tool` replies if needed.
    pending_requests: HashMap<String, Value>,
    /// Models reported by Claude Code in the `initialize` control response, if any.
    discovered_models: Vec<DiscoveredModel>,
    /// Slash commands announced before the first prompt, flushed when the prompt starts.
    pending_commands: Option<Vec<SlashCommand>>,
    /// Whether `LimitReached` has already been emitted for the active turn.
    limit_emitted: bool,
}

/// Formats a human-readable model label from a Claude Code `initialize` model entry.
fn model_label(
    value: &str,
    display_name: Option<&str>,
    description: Option<&str>,
    resolved: Option<&str>,
) -> String {
    let short_desc = description
        .and_then(|d| d.split(" · ").next())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if value == "default" {
        return match short_desc {
            Some(v) => format!("Default ({v})"),
            None => display_name.unwrap_or("Default").to_owned(),
        };
    }
    if let Some(short) = short_desc
        && short.chars().any(|c| c.is_ascii_digit())
    {
        return if short.starts_with("Claude ") {
            short.to_owned()
        } else {
            format!("Claude {short}")
        };
    }
    if let Some(name) = display_name.filter(|s| !s.is_empty()) {
        if let Some(res) = resolved.filter(|r| !r.is_empty() && *r != value)
            && !name.chars().any(|c| c.is_ascii_digit())
        {
            return format!("{name} ({res})");
        }
        return name.to_owned();
    }
    resolved.unwrap_or(value).to_owned()
}

/// Extracts available models from a Claude Code `initialize` `control_response.response.response`.
pub fn extract_models(response: &Value) -> Vec<DiscoveredModel> {
    let Some(items) = response["models"].as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let Some(id) = item["value"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        if out.iter().any(|m: &DiscoveredModel| m.id == id) {
            continue;
        }
        let label = model_label(
            id,
            item["displayName"].as_str(),
            item["description"].as_str(),
            item["resolvedModel"].as_str(),
        );
        out.push(DiscoveredModel {
            id: id.to_owned(),
            label,
        });
    }
    if let Some(pos) = out.iter().position(|m| m.id == "default")
        && pos > 0
    {
        let item = out.remove(pos);
        out.insert(0, item);
    }
    out
}

impl Session {
    /// Creates an idle session.
    pub fn new(options: Options) -> Self {
        Session {
            phase: Phase::Idle,
            next_req: 0,
            init_req_id: None,
            session_id: options.resume,
            text: String::new(),
            streamed_text: false,
            started_tools: HashSet::new(),
            finished_tools: HashSet::new(),
            tool_order: Vec::new(),
            pending_requests: HashMap::new(),
            discovered_models: Vec::new(),
            pending_commands: None,
            limit_emitted: false,
        }
    }

    /// Models reported by Claude Code during `initialize`, if any.
    pub fn discovered_models(&self) -> &[DiscoveredModel] {
        &self.discovered_models
    }

    /// Whether a prompt turn is currently in flight.
    pub fn is_prompting(&self) -> bool {
        self.phase == Phase::Prompting
    }

    fn next_request_id(&mut self) -> String {
        self.next_req += 1;
        format!("req-{}", self.next_req)
    }

    /// Starts the session by sending the `initialize` control request.
    pub fn start(&mut self) -> Vec<Action> {
        self.phase = Phase::Initializing;
        let id = self.next_request_id();
        self.init_req_id = Some(id.clone());
        vec![Action::Write(
            json!({
                "type": "control_request",
                "request_id": id,
                "request": { "subtype": "initialize" }
            })
            .to_string(),
        )]
    }

    /// Sends a user prompt turn.
    pub fn prompt(&mut self, text: &str) -> Vec<Action> {
        self.prompt_with_attachments(text, &[])
    }

    /// Sends a user prompt turn with optional image/file attachments.
    pub fn prompt_with_attachments(
        &mut self,
        text: &str,
        attachments: &[Attachment],
    ) -> Vec<Action> {
        if self.phase != Phase::Ready {
            return vec![Action::Failed("the session is not ready".into())];
        }
        self.phase = Phase::Prompting;
        self.text.clear();
        self.streamed_text = false;
        self.started_tools.clear();
        self.finished_tools.clear();
        self.tool_order.clear();
        self.pending_requests.clear();
        self.limit_emitted = false;

        let mut out = Vec::new();
        if let Some(commands) = self.pending_commands.take()
            && !commands.is_empty()
        {
            out.push(Action::Emit(Event::CommandsAvailable { commands }));
        }

        if attachments.is_empty() {
            out.push(Action::Write(
                json!({
                    "type": "user",
                    "message": { "role": "user", "content": text }
                })
                .to_string(),
            ));
            return out;
        }

        let mut blocks = Vec::new();
        let mut file_notes = Vec::new();
        for a in attachments {
            if matches!(
                a.mime_type.as_str(),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            ) && let Some(data) = &a.data
            {
                blocks.push(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": a.mime_type,
                        "data": data,
                    }
                }));
            } else {
                file_notes.push(format!(
                    "Attached file: {} ({}) at {}",
                    a.name, a.mime_type, a.host_path
                ));
            }
        }
        let combined = match (text.is_empty(), file_notes.is_empty()) {
            (false, true) => text.to_owned(),
            (true, false) => file_notes.join("\n"),
            (false, false) => format!("{text}\n\n{}", file_notes.join("\n")),
            (true, true) => String::new(),
        };
        if !combined.is_empty() {
            blocks.push(json!({ "type": "text", "text": combined }));
        }
        out.push(Action::Write(
            json!({
                "type": "user",
                "message": { "role": "user", "content": blocks }
            })
            .to_string(),
        ));
        out
    }

    /// Sends a steering message to Claude Code while a turn is running (ADR 0028).
    /// Resets accumulated text so `TextDone` at turn end covers only text emitted after
    /// the steer.
    pub fn steer(&mut self, text: &str) -> Vec<Action> {
        if self.phase != Phase::Prompting {
            return Vec::new();
        }
        self.text.clear();
        self.streamed_text = false;
        vec![Action::Write(
            json!({
                "type": "user",
                "message": { "role": "user", "content": text }
            })
            .to_string(),
        )]
    }

    /// Asks Claude Code to interrupt the active turn.
    pub fn cancel(&mut self) -> Vec<Action> {
        if self.phase != Phase::Prompting {
            return Vec::new();
        }
        let id = self.next_request_id();
        vec![Action::Write(
            json!({
                "type": "control_request",
                "request_id": id,
                "request": { "subtype": "interrupt" }
            })
            .to_string(),
        )]
    }

    /// Sends the `control_response` for a `can_use_tool` permission request.
    pub fn decide(&mut self, request_id: &str, detail: &Value, decision: Decision) -> Vec<Action> {
        self.pending_requests.remove(request_id);
        let input = detail.get("input").cloned().unwrap_or_else(|| json!({}));
        let body = match decision {
            Decision::Allow => json!({
                "behavior": "allow",
                "updatedInput": input,
            }),
            Decision::AllowAlways => {
                let mut obj = json!({
                    "behavior": "allow",
                    "updatedInput": input,
                });
                if let Some(suggestions) = detail.get("permission_suggestions")
                    && suggestions.as_array().is_some_and(|a| !a.is_empty())
                {
                    obj["updatedPermissions"] = suggestions.clone();
                }
                obj
            }
            Decision::Deny => json!({
                "behavior": "deny",
                "message": "Denied by user",
            }),
            Decision::Cancelled => json!({
                "behavior": "deny",
                "message": "Cancelled",
                "interrupt": true,
            }),
        };
        vec![Action::Write(
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": body,
                }
            })
            .to_string(),
        )]
    }

    /// Processes one stdout line from Claude Code.
    pub fn on_line(&mut self, line: &str) -> Vec<Action> {
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        if let Some(sid) = msg.get("session_id").and_then(Value::as_str)
            && !sid.is_empty()
        {
            self.session_id = Some(sid.to_owned());
        }
        match msg.get("type").and_then(Value::as_str) {
            Some("control_response") => self.on_control_response(&msg),
            Some("control_request") => self.on_control_request(&msg),
            Some("system") => self.on_system(&msg),
            Some("rate_limit_event") => self.on_rate_limit_event(&msg),
            Some("stream_event") => self.on_stream_event(&msg),
            Some("assistant") => self.on_assistant(&msg),
            Some("user") => self.on_user(&msg),
            Some("result") => self.on_result(&msg),
            _ => Vec::new(),
        }
    }

    fn on_system(&mut self, msg: &Value) -> Vec<Action> {
        if msg["subtype"].as_str() != Some("init") {
            return Vec::new();
        }
        let commands = extract_slash_commands(&msg["slash_commands"]);
        if commands.is_empty() {
            return Vec::new();
        }
        if self.phase == Phase::Prompting {
            vec![Action::Emit(Event::CommandsAvailable { commands })]
        } else {
            self.pending_commands = Some(commands);
            Vec::new()
        }
    }

    fn on_rate_limit_event(&mut self, msg: &Value) -> Vec<Action> {
        if self.phase != Phase::Prompting {
            return Vec::new();
        }
        let info = &msg["rate_limit_info"];
        if info["status"].as_str() != Some("rejected") {
            return Vec::new();
        }
        let reset_at = info["resetsAt"]
            .as_i64()
            .or_else(|| info["resets_at"].as_i64())
            .and_then(unix_secs_to_iso)
            .or_else(|| {
                info["resetsAt"]
                    .as_str()
                    .or_else(|| info["resets_at"].as_str())
                    .map(str::to_owned)
            });
        let message = info["message"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("Claude Code usage limit reached")
            .to_owned();
        self.limit_emitted = true;
        vec![Action::Emit(Event::LimitReached { reset_at, message })]
    }

    fn on_control_response(&mut self, msg: &Value) -> Vec<Action> {
        let resp = &msg["response"];
        let req_id = resp["request_id"].as_str();
        if self.phase == Phase::Initializing && req_id == self.init_req_id.as_deref() {
            self.init_req_id = None;
            if resp["subtype"].as_str() == Some("error") {
                self.phase = Phase::Idle;
                let err = resp["error"]
                    .as_str()
                    .unwrap_or("Claude Code initialization failed")
                    .to_owned();
                return vec![Action::Failed(err)];
            }
            self.discovered_models = extract_models(&resp["response"]);
            let cmds = extract_slash_commands(&resp["response"]["commands"]);
            if !cmds.is_empty() {
                self.pending_commands = Some(cmds);
            }
            self.phase = Phase::Ready;
            let session = self
                .session_id
                .clone()
                .unwrap_or_else(|| "claude-session".to_owned());
            return vec![Action::Ready { session }];
        }
        Vec::new()
    }

    fn on_control_request(&mut self, msg: &Value) -> Vec<Action> {
        let Some(request_id) = msg["request_id"].as_str() else {
            return Vec::new();
        };
        let req = &msg["request"];
        if req["subtype"].as_str() == Some("can_use_tool") && self.phase == Phase::Prompting {
            let summary = req["description"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| req["display_name"].as_str().filter(|s| !s.is_empty()))
                .or_else(|| req["tool_name"].as_str().filter(|s| !s.is_empty()))
                .unwrap_or("Use a tool")
                .to_owned();
            self.pending_requests
                .insert(request_id.to_owned(), req.clone());
            return vec![Action::Approve {
                request_id: request_id.to_owned(),
                kind: "tool".into(),
                summary,
                detail: req.clone(),
            }];
        }
        let subtype = req["subtype"].as_str().unwrap_or("unknown");
        vec![Action::Write(
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": request_id,
                    "error": format!("unsupported control_request subtype: {subtype}"),
                }
            })
            .to_string(),
        )]
    }

    fn on_stream_event(&mut self, msg: &Value) -> Vec<Action> {
        if self.phase != Phase::Prompting {
            return Vec::new();
        }
        let ev = &msg["event"];
        match ev["type"].as_str() {
            Some("message_start") => {
                self.streamed_text = false;
                Vec::new()
            }
            Some("content_block_delta") => {
                let delta = &ev["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = delta["text"].as_str().unwrap_or_default().to_owned();
                        if text.is_empty() {
                            return Vec::new();
                        }
                        self.streamed_text = true;
                        self.text.push_str(&text);
                        vec![Action::Emit(Event::TextDelta { text })]
                    }
                    Some("thinking_delta") => {
                        let text = delta["thinking"].as_str().unwrap_or_default().to_owned();
                        if text.is_empty() {
                            return Vec::new();
                        }
                        vec![Action::Emit(Event::ReasoningDelta { text })]
                    }
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    fn on_assistant(&mut self, msg: &Value) -> Vec<Action> {
        if self.phase != Phase::Prompting {
            return Vec::new();
        }
        let Some(blocks) = msg["message"]["content"].as_array() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for block in blocks {
            match block["type"].as_str() {
                Some("tool_use") => {
                    if let Some(id) = block["id"].as_str()
                        && self.started_tools.insert(id.to_owned())
                    {
                        self.tool_order.push(id.to_owned());
                        let name = block["name"].as_str().unwrap_or("tool").to_owned();
                        let input = if block["input"].is_null() {
                            json!({})
                        } else {
                            block["input"].clone()
                        };
                        out.push(Action::Emit(Event::ToolStarted {
                            call: id.to_owned(),
                            name,
                            input,
                        }));
                    }
                }
                Some("text") if !self.streamed_text => {
                    let text = block["text"].as_str().unwrap_or_default().to_owned();
                    if !text.is_empty() {
                        self.text.push_str(&text);
                        out.push(Action::Emit(Event::TextDelta { text }));
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn on_user(&mut self, msg: &Value) -> Vec<Action> {
        if self.phase != Phase::Prompting {
            return Vec::new();
        }
        let Some(blocks) = msg["message"]["content"].as_array() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for block in blocks {
            if block["type"].as_str() != Some("tool_result") {
                continue;
            }
            let Some(call) = block["tool_use_id"].as_str() else {
                continue;
            };
            if !self.finished_tools.insert(call.to_owned()) {
                continue;
            }
            let text = tool_result_text(&block["content"]);
            if !text.is_empty() {
                out.push(Action::Emit(Event::ToolOutput {
                    call: call.to_owned(),
                    chunk: text.clone(),
                }));
            }
            let ok = block["is_error"] != Value::Bool(true);
            let result = if text.is_empty() {
                None
            } else {
                Some(Value::String(text))
            };
            out.push(Action::Emit(Event::ToolFinished {
                call: call.to_owned(),
                ok,
                result,
            }));
        }
        out
    }

    fn on_result(&mut self, msg: &Value) -> Vec<Action> {
        if self.phase != Phase::Prompting {
            return Vec::new();
        }
        self.phase = Phase::Ready;
        let mut out = Vec::new();
        for call in std::mem::take(&mut self.tool_order) {
            if !self.finished_tools.contains(&call) {
                out.push(Action::Emit(Event::ToolFinished {
                    call,
                    ok: false,
                    result: None,
                }));
            }
        }
        if !self.text.is_empty() {
            out.push(Action::Emit(Event::TextDone {
                text: std::mem::take(&mut self.text),
            }));
        }
        let usage = &msg["usage"];
        let cost = msg["total_cost_usd"].as_f64();
        if usage.is_object() || cost.is_some() {
            let input_tokens = usage["input_tokens"].as_u64().unwrap_or(0)
                + usage["cache_creation_input_tokens"].as_u64().unwrap_or(0)
                + usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
            let output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
            out.push(Action::Emit(Event::Usage {
                input_tokens,
                output_tokens,
                cost,
            }));
        }
        if msg["is_error"] == Value::Bool(true) && msg["subtype"].as_str() != Some("success") {
            let err = msg["result"]
                .as_str()
                .unwrap_or("Claude Code returned an error")
                .to_owned();
            if !self.limit_emitted && is_limit_message(&err) {
                self.limit_emitted = true;
                out.push(Action::Emit(Event::LimitReached {
                    reset_at: None,
                    message: err.clone(),
                }));
            }
            out.push(Action::Failed(err));
        } else {
            out.push(Action::TurnDone);
        }
        out
    }
}

fn extract_slash_commands(value: &Value) -> Vec<SlashCommand> {
    let Some(items) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let cmd = match item {
            Value::String(s) => {
                let name = s.trim().trim_start_matches('/');
                if name.is_empty() {
                    continue;
                }
                SlashCommand {
                    name: name.to_owned(),
                    description: None,
                    hint: None,
                }
            }
            Value::Object(_) => {
                let Some(name) = item["name"]
                    .as_str()
                    .map(str::trim)
                    .map(|s| s.trim_start_matches('/'))
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let description = item["description"]
                    .as_str()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let hint = item["argumentHint"]
                    .as_str()
                    .or_else(|| item["hint"].as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                SlashCommand {
                    name: name.to_owned(),
                    description,
                    hint,
                }
            }
            _ => continue,
        };
        if !out
            .iter()
            .any(|existing: &SlashCommand| existing.name == cmd.name)
        {
            out.push(cmd);
        }
    }
    out
}

fn is_limit_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("rate limit")
        || lower.contains("usage limit")
        || lower.contains("quota exceeded")
        || lower.contains("overloaded")
        || lower.contains("hit your limit")
        || lower.contains("429")
}

/// Formats a non-negative Unix timestamp in seconds as RFC 3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`).
fn unix_secs_to_iso(secs: i64) -> Option<String> {
    if secs < 0 {
        return None;
    }
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let hour = rem / 3_600;
    let min = (rem % 3_600) / 60;
    let sec = rem % 60;
    // Howard Hinnant's civil-from-days algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(m <= 2);
    Some(format!(
        "{year:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z"
    ))
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                if item["type"] == "text" {
                    item["text"].as_str()
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn text_and_permission_round_trip() {
        let mut s = Session::new(Options::default());
        let start = s.start();
        let [Action::Write(init)] = start.as_slice() else {
            panic!("{start:?}");
        };
        let init_v: Value = serde_json::from_str(init).unwrap();
        assert_eq!(init_v["type"], "control_request");
        assert_eq!(init_v["request"]["subtype"], "initialize");

        assert!(
            s.on_line(r#"{"type":"system","subtype":"ui_invalidate","session_id":"sess-42"}"#)
                .is_empty()
        );
        let ready = s.on_line(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"req-1","response":{}}}"#,
        );
        assert_eq!(
            ready,
            [Action::Ready {
                session: "sess-42".into()
            }]
        );

        let prompt = s.prompt("Create hello.txt");
        let [Action::Write(u)] = prompt.as_slice() else {
            panic!("{prompt:?}");
        };
        let uv: Value = serde_json::from_str(u).unwrap();
        assert_eq!(uv["type"], "user");
        assert_eq!(uv["message"]["content"], "Create hello.txt");

        let ev = s.on_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Plan"}}}"#,
        );
        assert_eq!(
            ev,
            [Action::Emit(Event::ReasoningDelta {
                text: "Plan".into()
            })]
        );

        let started = s.on_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"touch hi"}}]}}"#,
        );
        assert_eq!(
            started,
            [Action::Emit(Event::ToolStarted {
                call: "toolu_1".into(),
                name: "Bash".into(),
                input: json!({"command": "touch hi"}),
            })]
        );

        let ask = s.on_line(
            r#"{"type":"control_request","request_id":"perm-1","request":{"subtype":"can_use_tool","tool_name":"Bash","description":"touch hi","input":{"command":"touch hi"},"permission_suggestions":[{"type":"setMode","mode":"acceptEdits"}]}}"#,
        );
        let [
            Action::Approve {
                request_id,
                summary,
                detail,
                ..
            },
        ] = ask.as_slice()
        else {
            panic!("{ask:?}");
        };
        assert_eq!(request_id, "perm-1");
        assert_eq!(summary, "touch hi");

        let reply = s.decide(request_id, detail, Decision::AllowAlways);
        let [Action::Write(rline)] = reply.as_slice() else {
            panic!("{reply:?}");
        };
        let rv: Value = serde_json::from_str(rline).unwrap();
        assert_eq!(rv["response"]["response"]["behavior"], "allow");
        assert!(rv["response"]["response"]["updatedPermissions"].is_array());

        let tool_res = s.on_line(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok","is_error":false}]}}"#,
        );
        assert_eq!(
            tool_res,
            [
                Action::Emit(Event::ToolOutput {
                    call: "toolu_1".into(),
                    chunk: "ok".into(),
                }),
                Action::Emit(Event::ToolFinished {
                    call: "toolu_1".into(),
                    ok: true,
                    result: Some(json!("ok")),
                }),
            ]
        );

        let td = s.on_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done."}}}"#,
        );
        assert_eq!(
            td,
            [Action::Emit(Event::TextDelta {
                text: "Done.".into()
            })]
        );

        let done = s.on_line(
            r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.01,"usage":{"input_tokens":10,"output_tokens":5}}"#,
        );
        assert_eq!(
            done,
            [
                Action::Emit(Event::TextDone {
                    text: "Done.".into()
                }),
                Action::Emit(Event::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cost: Some(0.01),
                }),
                Action::TurnDone,
            ]
        );
    }

    #[test]
    fn cancel_sends_interrupt() {
        let mut s = Session::new(Options::default());
        s.start();
        s.on_line(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"req-1","response":{}}}"#,
        );
        s.prompt("hello");
        let out = s.cancel();
        let [Action::Write(line)] = out.as_slice() else {
            panic!("{out:?}");
        };
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["type"], "control_request");
        assert_eq!(v["request"]["subtype"], "interrupt");
    }

    #[test]
    fn extracts_versioned_models_from_initialize_response() {
        let mut s = Session::new(Options::default());
        s.start();
        let ready = s.on_line(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"req-1","response":{"models":[{"value":"default","resolvedModel":"claude-opus-5-5","displayName":"Default (recommended)","description":"Opus 5.5 · Best for everyday, complex tasks"},{"value":"opus","resolvedModel":"claude-opus-5-5","displayName":"Opus","description":"Opus 5.5 · Best for everyday, complex tasks"},{"value":"claude-fable-5-2[1m]","resolvedModel":"claude-fable-5-2","displayName":"Fable","description":"Fable 5.2 · Most capable"},{"value":"sonnet","resolvedModel":"claude-sonnet-5-5","displayName":"Sonnet","description":"Sonnet 5.5 · Efficient"},{"value":"haiku","resolvedModel":"claude-haiku-5-5","displayName":"Haiku","description":"Haiku 5.5 · Fastest"}]}}}"#,
        );
        assert!(matches!(ready.as_slice(), [Action::Ready { .. }]));
        assert_eq!(
            s.discovered_models(),
            &[
                DiscoveredModel {
                    id: "default".into(),
                    label: "Default (Opus 5.5)".into()
                },
                DiscoveredModel {
                    id: "opus".into(),
                    label: "Claude Opus 5.5".into()
                },
                DiscoveredModel {
                    id: "claude-fable-5-2[1m]".into(),
                    label: "Claude Fable 5.2".into()
                },
                DiscoveredModel {
                    id: "sonnet".into(),
                    label: "Claude Sonnet 5.5".into()
                },
                DiscoveredModel {
                    id: "haiku".into(),
                    label: "Claude Haiku 5.5".into()
                },
            ]
        );
    }

    #[test]
    fn multimodal_prompt_encodes_images_and_file_paths() {
        let mut s = Session::new(Options::default());
        s.start();
        s.on_line(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"req-1","response":{}}}"#,
        );
        let out = s.prompt_with_attachments(
            "what is this?",
            &[
                Attachment {
                    name: "shot.png".into(),
                    mime_type: "image/png".into(),
                    host_path: "/tmp/0-shot.png".into(),
                    data: Some("iVBORw0KGgo=".into()),
                },
                Attachment {
                    name: "spec.md".into(),
                    mime_type: "text/markdown".into(),
                    host_path: "/tmp/1-spec.md".into(),
                    data: None,
                },
            ],
        );
        let [Action::Write(line)] = out.as_slice() else {
            panic!("{out:?}");
        };
        let v: Value = serde_json::from_str(line).unwrap();
        let content = v["message"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        assert_eq!(content[0]["source"]["data"], "iVBORw0KGgo=");
        assert_eq!(content[1]["type"], "text");
        let text = content[1]["text"].as_str().unwrap();
        assert!(text.contains("what is this?"));
        assert!(text.contains("/tmp/1-spec.md"));
    }

    #[test]
    fn steer_writes_user_message_mid_turn_and_resets_accumulated_text() {
        let mut s = Session::new(Options::default());
        s.start();
        s.on_line(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"req-1","response":{}}}"#,
        );
        s.prompt("start");
        assert!(s.is_prompting());
        s.on_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"before"}}}"#,
        );
        let steered = s.steer("use the helper");
        let [Action::Write(line)] = steered.as_slice() else {
            panic!("{steered:?}");
        };
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["content"], "use the helper");
        s.on_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"after"}}}"#,
        );
        let done = s.on_line(
            r#"{"type":"result","subtype":"success","is_error":false,"usage":{"input_tokens":5,"output_tokens":2}}"#,
        );
        assert_eq!(
            done[0],
            Action::Emit(Event::TextDone {
                text: "after".into()
            })
        );
        assert!(!s.is_prompting());
    }

    #[test]
    fn system_init_emits_slash_commands() {
        let mut s = Session::new(Options::default());
        s.start();
        s.on_line(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"req-1","response":{}}}"#,
        );
        s.prompt("hi");
        let actions = s.on_line(
            r#"{"type":"system","subtype":"init","slash_commands":["/review","compact","compact"]}"#,
        );
        assert_eq!(
            actions,
            [Action::Emit(Event::CommandsAvailable {
                commands: vec![
                    SlashCommand {
                        name: "review".into(),
                        description: None,
                        hint: None,
                    },
                    SlashCommand {
                        name: "compact".into(),
                        description: None,
                        hint: None,
                    },
                ],
            })]
        );
    }

    #[test]
    fn rate_limit_event_emits_limit_reached_with_iso_reset() {
        let mut s = Session::new(Options::default());
        s.start();
        s.on_line(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"req-1","response":{}}}"#,
        );
        s.prompt("hi");
        // Allowed warnings do not trigger limit_reached.
        assert!(
            s.on_line(
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1735689600}}"#
            )
            .is_empty()
        );
        // Rejected emits LimitReached with ISO reset time.
        let actions = s.on_line(
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1735689600,"message":"5h usage limit hit"}}"#,
        );
        assert_eq!(
            actions,
            [Action::Emit(Event::LimitReached {
                reset_at: Some("2025-01-01T00:00:00Z".into()),
                message: "5h usage limit hit".into(),
            })]
        );
        // Subsequent error result does not duplicate LimitReached.
        let done = s.on_line(
            r#"{"type":"result","subtype":"error","is_error":true,"result":"Usage limit reached"}"#,
        );
        assert_eq!(done, [Action::Failed("Usage limit reached".into())]);
    }

    #[test]
    fn error_result_rate_limit_emits_limit_reached() {
        let mut s = Session::new(Options::default());
        s.start();
        s.on_line(
            r#"{"type":"control_response","response":{"subtype":"success","request_id":"req-1","response":{}}}"#,
        );
        s.prompt("hi");
        let done = s.on_line(
            r#"{"type":"result","subtype":"error","is_error":true,"result":"429 rate limit exceeded"}"#,
        );
        assert_eq!(
            done,
            [
                Action::Emit(Event::LimitReached {
                    reset_at: None,
                    message: "429 rate limit exceeded".into(),
                }),
                Action::Failed("429 rate limit exceeded".into()),
            ]
        );
    }
}
