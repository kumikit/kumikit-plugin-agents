//! End-to-end tests of every declarative provider (exit criterion E1), against a real
//! `kumikit serve` built from the pinned kumikit checkout (`KUMIKIT_DIR`):
//! - each `providers/<name>` packs (manifest and README only, as published), installs and
//!   enables next to `kumikit.provider-acp`;
//! - each recorded transcript replays through `fake-acp-agent replay` and provider-acp in
//!   that core, answering approvals as the recorded user did, and gives the event kinds of
//!   `tests/expected/<transcript>.json` between the kernel's `turn.started` and `turn.ended`.
//!
//! The ignored `live_*` tests send one cheap prompt to each real installed CLI
//! (`cargo test -p kumikit-agents-e2e -- --ignored`); they use your sign-in.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use kumikit_agents_harness::{Provider, providers, transcripts};
use serde_json::{Value, json};
use tempfile::TempDir;

const DEADLINE: Duration = Duration::from_secs(60);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

/// What the tests need from a built kumikit checkout.
struct Bins {
    kumikit: PathBuf,
    fake_agent: PathBuf,
    acp_dir: PathBuf,
    acp_wasm: PathBuf,
}

impl Bins {
    fn locate() -> Self {
        let k = std::env::var_os("KUMIKIT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| repo_root().parent().unwrap().join("kumikit"));
        let b = Bins {
            kumikit: k.join("target/debug").join(exe("kumikit")),
            fake_agent: k
                .join("target/kumikit-test/release")
                .join(exe("fake-acp-agent")),
            acp_dir: k.join("plugins/provider-acp"),
            acp_wasm: k.join("target/kumikit-test/wasm32-wasip2/release/kumikit_provider_acp.wasm"),
        };
        for f in [&b.kumikit, &b.fake_agent, &b.acp_wasm] {
            assert!(
                f.is_file(),
                "{} is missing: set KUMIKIT_DIR to a kumikit checkout built with \
                 `cargo build -p kumikit-core -p kumikit-test-plugins`",
                f.display()
            );
        }
        b
    }

    fn pack(&self, dir: &Path, out: &Path) {
        let o = Command::new(&self.kumikit)
            .args(["plugin", "pack"])
            .arg(dir)
            .arg("-o")
            .arg(out)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "kumikit plugin pack {}: {}",
            dir.display(),
            String::from_utf8_lossy(&o.stderr)
        );
    }
}

/// A core of its own (socket and data directory in a temp dir) and a client connected
/// through `kumikit bridge`.
struct Core {
    tmp: TempDir,
    workspace: PathBuf,
    core: Child,
    bridge: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Value>,
    next_id: u64,
    events: Vec<Value>,
}

impl Core {
    fn start(bins: &Bins, path_first: Option<&Path>) -> Self {
        let tmp = tempfile::Builder::new().prefix("kka").tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The core refuses a socket directory others can read.
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let socket = if cfg!(windows) {
            format!(
                r"\\.\pipe\kka-{}",
                tmp.path().file_name().unwrap().to_string_lossy()
            )
        } else {
            tmp.path().join("s.sock").to_string_lossy().into_owned()
        };
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("README.md"), "# Demo\n").unwrap();

        let mut cmd = Command::new(&bins.kumikit);
        cmd.args(["serve", "--socket", &socket, "--data-dir"])
            .arg(tmp.path().join("data"))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(dir) = path_first {
            let mut paths = vec![dir.to_path_buf()];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            cmd.env("PATH", std::env::join_paths(paths).unwrap());
        }
        let mut core = cmd.spawn().unwrap();
        let stdout = core.stdout.take().unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Keep reading after `ready`, so the core never writes to a closed pipe.
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.starts_with("ready") {
                    let _ = ready_tx.send(());
                }
            }
        });
        ready_rx
            .recv_timeout(DEADLINE)
            .expect("kumikit serve printed no `ready` in time");

        let mut bridge = Command::new(&bins.kumikit)
            .args(["bridge", "--socket", &socket])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = bridge.stdin.take().unwrap();
        let out = bridge.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let mut c = Core {
            tmp,
            workspace,
            core,
            bridge,
            stdin,
            rx,
            next_id: 0,
            events: Vec::new(),
        };
        c.call(
            "core.hello",
            json!({ "protocol": "0.9", "client": { "name": "kumikit-agents-e2e", "version": "0" } }),
        );
        c
    }

    fn recv(&mut self, deadline: Instant) -> Value {
        let left = deadline
            .checked_duration_since(Instant::now())
            .expect("deadline passed");
        self.rx
            .recv_timeout(left)
            .expect("no message from the core before the deadline")
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{req}").unwrap();
        self.stdin.flush().unwrap();
        let deadline = Instant::now() + DEADLINE;
        loop {
            let m = self.recv(deadline);
            if m["method"] == "events.event" {
                self.events.push(m["params"].clone());
            } else if m["id"] == json!(id) {
                assert!(m.get("error").is_none(), "{method}: {}", m["error"]);
                return m["result"].clone();
            }
        }
    }

    fn event(&mut self, topic: &str, deadline: Instant) -> Value {
        if let Some(i) = self.events.iter().position(|e| e["topic"] == topic) {
            return self.events.remove(i);
        }
        loop {
            let m = self.recv(deadline);
            if m["method"] == "events.event" {
                if m["params"]["topic"] == topic {
                    return m["params"].clone();
                }
                self.events.push(m["params"].clone());
            }
        }
    }

    fn install(&mut self, kkp: &Path) -> Value {
        let p = self.call("plugin.install", json!({ "path": kkp }))["plugin"].clone();
        self.call(
            "plugin.enable",
            json!({ "plugin": p["id"], "grant": p["requested"] }),
        )["plugin"]
            .clone()
    }

    /// Packs and installs `files` (name, source) as a plugin.
    fn install_files(&mut self, bins: &Bins, name: &str, files: &[(&str, &Path)]) -> Value {
        let dir = self.tmp.path().join("pkg").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for (to, from) in files {
            std::fs::copy(from, dir.join(to)).unwrap();
        }
        let kkp = self.tmp.path().join(format!("{name}.kkp"));
        bins.pack(&dir, &kkp);
        self.install(&kkp)
    }

    fn install_provider_acp(&mut self, bins: &Bins) {
        let (manifest, schema) = (
            bins.acp_dir.join("plugin.toml"),
            bins.acp_dir.join("acp.schema.json"),
        );
        self.install_files(
            bins,
            "provider-acp",
            &[
                ("plugin.toml", &manifest),
                ("acp.schema.json", &schema),
                ("plugin.wasm", &bins.acp_wasm),
            ],
        );
    }

    /// A provider as published: its manifest and README.
    fn install_provider(&mut self, bins: &Bins, p: &Provider) -> Value {
        let (manifest, readme) = (p.dir.join("plugin.toml"), p.dir.join("README.md"));
        self.install_files(
            bins,
            &p.name,
            &[("plugin.toml", &manifest), ("README.md", &readme)],
        )
    }

    fn open_workspace(&mut self) -> String {
        let ws = self.workspace.clone();
        self.call("workspace.open", json!({ "path": ws }))["workspace"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// Starts a thread on `provider`, sends `text` and returns its topic.
    fn send(&mut self, workspace: &str, provider: &str, text: &str) -> String {
        let t = self.call(
            "thread.create",
            json!({ "workspace": workspace, "provider": provider }),
        );
        let id = t["thread"]["id"].as_str().unwrap().to_owned();
        let topic = format!("thread:{id}");
        self.call("events.subscribe", json!({ "topic": topic, "after": 0 }));
        self.call(
            "thread.send",
            json!({ "thread": id, "input": { "text": text } }),
        );
        topic
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.bridge.kill();
        let _ = self.bridge.wait();
        let _ = self.core.kill();
        let _ = self.core.wait();
    }
}

/// The recorded user's answers to the agent's permission requests, in order.
fn recorded_decisions(transcript: &Path) -> Vec<&'static str> {
    let mut options: HashMap<String, Value> = HashMap::new();
    let mut out = Vec::new();
    for line in std::fs::read_to_string(transcript)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        let e: Value = serde_json::from_str(line).unwrap();
        let m = &e["message"];
        if e["from"] == "agent" && m["method"] == "session/request_permission" {
            options.insert(m["id"].to_string(), m["params"]["options"].clone());
        } else if e["from"] == "client" && m.get("method").is_none() {
            let chosen = &m["result"]["outcome"]["optionId"];
            let kind = options[&m["id"].to_string()]
                .as_array()
                .and_then(|o| o.iter().find(|o| &o["optionId"] == chosen))
                .and_then(|o| o["kind"].as_str())
                .unwrap_or("reject_once");
            out.push(match kind {
                "allow_once" => "allow",
                "allow_always" => "allow_always",
                _ => "deny",
            });
        }
    }
    out
}

/// The kernel event kinds of a turn: the adapter's expected events between `turn.started`
/// and `turn.ended`.
fn expected_kinds(path: &Path) -> Vec<String> {
    let log: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut out = vec!["turn.started".to_owned()];
    for e in log {
        if e.get("approve").is_some() {
            out.extend([
                "approval.requested".to_owned(),
                "approval.resolved".to_owned(),
            ]);
        } else if let Some(kind) = e["event"].as_str() {
            out.push(if kind == "plugin" {
                "plugin.event".to_owned()
            } else {
                kind.replacen('_', ".", 1)
            });
        }
    }
    out.push("turn.ended".into());
    out
}

/// A manifest-only plugin with one provider per transcript of `p`, each replaying it with
/// `p`'s ACP options.
fn replay_manifest(p: &Provider, id: &str) -> String {
    let mut entries = Vec::new();
    for (stem, path) in transcripts(p) {
        let file = std::fs::canonicalize(path)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "\\\\");
        let mut extra = String::new();
        if let Some(m) = &p.mode {
            extra.push_str(&format!(", mode = \"{m}\""));
        }
        if let Some(a) = &p.auth_method {
            extra.push_str(&format!(", auth-method = \"{a}\""));
        }
        entries.push(format!(
            "  {{ id = \"{stem}\", name = \"{stem}\", acp = {{ command = \"fake-acp-agent\", args = [\"replay\", \"{file}\"]{extra} }} }},"
        ));
    }
    format!(
        "id = \"{id}\"\nname = \"Replay\"\nversion = \"0.1.0\"\nkind = [\"provider\"]\napi = \"^0.2\"\n\
         [permissions]\nspawn = [\"fake-acp-agent\"]\ntools = true\n[contributes]\nproviders = [\n{}\n]\n",
        entries.join("\n")
    )
}

#[test]
fn every_provider_installs_and_its_recordings_replay_through_a_real_core() {
    let bins = Bins::locate();
    let mut core = Core::start(&bins, bins.fake_agent.parent());
    core.install_provider_acp(&bins);
    let ws = core.open_workspace();

    let all = providers(&repo_root()).unwrap();
    assert!(!all.is_empty());
    for p in &all {
        let installed = core.install_provider(&bins, p);
        assert!(
            installed["contributes"]["providers"]
                .as_array()
                .is_some_and(|a| !a.is_empty()),
            "{}: no providers in {installed}",
            p.name
        );

        let id = format!("test.replay-{}", p.name);
        let manifest = core.tmp.path().join(format!("{}.toml", p.name));
        std::fs::write(&manifest, replay_manifest(p, &id)).unwrap();
        core.install_files(
            &bins,
            &format!("replay-{}", p.name),
            &[("plugin.toml", &manifest)],
        );

        for (stem, path) in transcripts(p) {
            let expected = p.dir.join("tests/expected").join(format!("{stem}.json"));
            let mut answers = recorded_decisions(&path).into_iter();
            let topic = core.send(&ws, &format!("{id}/{stem}"), "replay");
            let deadline = Instant::now() + DEADLINE;
            let mut kinds = Vec::new();
            loop {
                let e = core.event(&topic, deadline);
                let kind = e["kind"].as_str().unwrap().to_owned();
                if kind == "approval.requested" {
                    let answer = answers
                        .next()
                        .unwrap_or_else(|| panic!("{stem}: an approval no one recorded"));
                    let approval = e["data"]["approval"]["id"].clone();
                    core.call(
                        "thread.respondApproval",
                        json!({ "approval": approval, "decision": answer }),
                    );
                }
                kinds.push(kind.clone());
                if kind == "turn.ended" {
                    assert_eq!(e["data"]["outcome"], "done", "{stem}: {e}");
                    break;
                }
            }
            assert_eq!(kinds, expected_kinds(&expected), "{stem}");
        }
    }
}

/// One prompt through the real CLI of `name`, which must answer with the asked words.
fn live(name: &str) {
    let bins = Bins::locate();
    let mut core = Core::start(&bins, None);
    core.install_provider_acp(&bins);
    let p = providers(&repo_root())
        .unwrap()
        .into_iter()
        .find(|p| p.name == name)
        .unwrap();
    let installed = core.install_provider(&bins, &p);
    let provider = format!(
        "{}/{}",
        installed["id"].as_str().unwrap(),
        installed["contributes"]["providers"][0]["id"]
            .as_str()
            .unwrap()
    );
    let ws = core.open_workspace();
    let topic = core.send(
        &ws,
        &provider,
        "Reply with exactly the words: hello from kumikit. Do not use any tools.",
    );
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut text = String::new();
    loop {
        let e = core.event(&topic, deadline);
        match e["kind"].as_str() {
            Some("text.done") => text = e["data"]["text"].as_str().unwrap_or_default().to_owned(),
            Some("turn.ended") => {
                assert_eq!(e["data"]["outcome"], "done", "{e}");
                break;
            }
            _ => {}
        }
    }
    assert!(
        text.to_lowercase().contains("hello from kumikit"),
        "{name} said {text:?}"
    );
}

#[test]
#[ignore = "live: needs the Antigravity CLI (agy) and its sign-in"]
fn live_antigravity() {
    live("antigravity");
}

#[test]
#[ignore = "live: needs GitHub Copilot CLI and its sign-in"]
fn live_copilot() {
    live("copilot");
}

#[test]
#[ignore = "live: needs Cursor CLI and its sign-in"]
fn live_cursor() {
    live("cursor");
}

#[test]
#[ignore = "live: needs OpenCode"]
fn live_opencode() {
    live("opencode");
}

fn claude_recorded_decisions(transcript: &Path) -> Vec<&'static str> {
    let mut out = Vec::new();
    for line in std::fs::read_to_string(transcript)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
    {
        let e: Value = serde_json::from_str(line).unwrap();
        let m = &e["message"];
        if e["from"] == "client" && m["type"] == "control_response" {
            let resp = &m["response"]["response"];
            out.push(match resp["behavior"].as_str() {
                Some("allow") if resp.get("updatedPermissions").is_some() => "allow_always",
                Some("allow") => "allow",
                _ => "deny",
            });
        }
    }
    out
}

#[test]
fn claude_provider_and_deepseek_example_run_end_to_end() {
    let bins = Bins::locate();
    let root = repo_root();
    let claude_dir = root.join("providers/claude");
    let claude_wasm = root.join("target/wasm32-wasip2/release/kumikit_provider_claude.wasm");
    assert!(
        claude_wasm.is_file(),
        "{} is missing: run `cargo build --target wasm32-wasip2 --release -p kumikit-provider-claude`",
        claude_wasm.display()
    );

    let mut core = Core::start(&bins, bins.fake_agent.parent());
    let ws = core.open_workspace();

    // 1. Pack, install and enable kumikit.provider-claude.
    let claude_manifest = claude_dir.join("plugin.toml");
    let claude_schema = claude_dir.join("claude.schema.json");
    let claude_readme = claude_dir.join("README.md");
    let installed = core.install_files(
        &bins,
        "provider-claude",
        &[
            ("plugin.toml", &claude_manifest),
            ("claude.schema.json", &claude_schema),
            ("README.md", &claude_readme),
            ("plugin.wasm", &claude_wasm),
        ],
    );
    assert_eq!(installed["id"], "kumikit.provider-claude");
    assert!(
        installed["contributes"]["providers"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "provider-claude registered no direct provider: {installed}"
    );

    // 2. Pack, install and enable the DeepSeek community example (delegated `claude` backend).
    let ds_dir = claude_dir.join("examples/deepseek");
    let ds_manifest = ds_dir.join("plugin.toml");
    let ds_settings = ds_dir.join("settings.schema.json");
    let ds_readme = ds_dir.join("README.md");
    let ds = core.install_files(
        &bins,
        "provider-deepseek",
        &[
            ("plugin.toml", &ds_manifest),
            ("settings.schema.json", &ds_settings),
            ("README.md", &ds_readme),
        ],
    );
    assert_eq!(ds["id"], "community.provider-deepseek");
    assert_eq!(ds["contributes"]["settings"], true);
    let plist = core.call("provider.list", json!({}));
    assert!(
        plist["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "community.provider-deepseek/deepseek"),
        "deepseek not in provider.list: {plist}"
    );
    let ds_models = core.call(
        "provider.models",
        json!({ "provider": "community.provider-deepseek/deepseek" }),
    );
    assert!(
        ds_models["models"]
            .as_array()
            .is_some_and(|m| m.iter().any(|x| x["id"] == "deepseek-chat")),
        "deepseek models missing: {ds_models}"
    );
    let schema = core.call(
        "settings.schema",
        json!({ "plugin": "community.provider-deepseek" }),
    );
    assert_eq!(
        schema["schema"]["properties"]["apiKey"]["x-secret"],
        "deepseek-api-key"
    );

    // 3. Replay every recorded Claude Code stream-json transcript end-to-end through the
    //    `claude` backend and `fake-acp-agent replay`.
    let tdir = claude_dir.join("tests/transcripts");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&tdir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    assert!(!files.is_empty());

    let mut entries = Vec::new();
    for path in &files {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let file = std::fs::canonicalize(path)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "\\\\");
        entries.push(format!(
            "  {{ id = \"{stem}\", name = \"{stem}\", claude = {{ command = \"fake-acp-agent\", args = [\"replay\", \"{file}\"] }} }},"
        ));
    }
    let replay_manifest = core.tmp.path().join("claude-replay.toml");
    std::fs::write(
        &replay_manifest,
        format!(
            "id = \"test.replay-claude\"\nname = \"Replay Claude\"\nversion = \"0.1.0\"\nkind = [\"provider\"]\napi = \"^0.2\"\n\
             [permissions]\nspawn = [\"fake-acp-agent\"]\ntools = true\n[contributes]\nproviders = [\n{}\n]\n",
            entries.join("\n")
        ),
    )
    .unwrap();
    core.install_files(&bins, "replay-claude", &[("plugin.toml", &replay_manifest)]);

    for path in &files {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let expected = claude_dir
            .join("tests/expected")
            .join(format!("{stem}.json"));
        let mut answers = claude_recorded_decisions(path).into_iter();
        let topic = core.send(&ws, &format!("test.replay-claude/{stem}"), "replay");
        let deadline = Instant::now() + DEADLINE;
        let mut kinds = Vec::new();
        loop {
            let e = core.event(&topic, deadline);
            let kind = e["kind"].as_str().unwrap().to_owned();
            if kind == "approval.requested" {
                let answer = answers
                    .next()
                    .unwrap_or_else(|| panic!("{stem}: unexpected approval"));
                let approval = e["data"]["approval"]["id"].clone();
                core.call(
                    "thread.respondApproval",
                    json!({ "approval": approval, "decision": answer }),
                );
            }
            kinds.push(kind.clone());
            if kind == "turn.ended" {
                assert_eq!(e["data"]["outcome"], "done", "{stem}: {e}");
                break;
            }
        }
        assert_eq!(kinds, expected_kinds(&expected), "{stem}");
    }

    // 4. Mid-turn steering through `provider-claude` and `fake-acp-agent replay`.
    let steer_transcript = core.tmp.path().join("claude-steer.jsonl");
    let steer_lines = [
        json!({"from": "client", "message": {"type": "control_request", "request_id": "req-1", "request": {"subtype": "initialize"}}}),
        json!({"from": "agent", "message": {"type": "control_response", "response": {"subtype": "success", "request_id": "req-1", "response": {}}}}),
        json!({"from": "client", "message": {"type": "user", "message": {"role": "user", "content": "start"}}}),
        json!({"from": "agent", "message": {"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "working"}}}}),
        json!({"from": "client", "message": {"type": "user", "message": {"role": "user", "content": "go left"}}}),
        json!({"from": "agent", "message": {"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "steered ok"}}}}),
        json!({"from": "agent", "message": {"type": "result", "subtype": "success", "is_error": false, "usage": {"input_tokens": 5, "output_tokens": 2}}}),
    ];
    std::fs::write(
        &steer_transcript,
        steer_lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n",
    )
    .unwrap();
    let steer_file = steer_transcript.to_string_lossy().replace('\\', "\\\\");
    let steer_manifest = core.tmp.path().join("claude-steer.toml");
    std::fs::write(
        &steer_manifest,
        format!(
            "id = \"test.steer-claude\"\nname = \"Steer Claude\"\nversion = \"0.1.0\"\nkind = [\"provider\"]\napi = \"^0.2\"\n\
             [permissions]\nspawn = [\"fake-acp-agent\"]\n[contributes]\nproviders = [\n\
               {{ id = \"steer\", name = \"Steer\", claude = {{ command = \"fake-acp-agent\", args = [\"replay\", \"{steer_file}\"] }} }},\n\
             ]\n"
        ),
    )
    .unwrap();
    core.install_files(&bins, "steer-claude", &[("plugin.toml", &steer_manifest)]);
    let models = core.call(
        "provider.models",
        json!({ "provider": "test.steer-claude/steer" }),
    );
    assert!(
        models["models"][0]["capabilities"]
            .as_array()
            .is_some_and(|c| c.iter().any(|x| x == "steer")),
        "provider-claude models must report steer capability: {models}"
    );
    let created = core.call(
        "thread.create",
        json!({ "workspace": ws, "provider": "test.steer-claude/steer" }),
    );
    let thread = created["thread"]["id"].as_str().unwrap().to_owned();
    let topic = format!("thread:{thread}");
    core.call("events.subscribe", json!({ "topic": topic, "after": 0 }));
    core.call(
        "thread.send",
        json!({ "thread": thread, "input": { "text": "start" } }),
    );
    let deadline = Instant::now() + DEADLINE;
    loop {
        let e = core.event(&topic, deadline);
        if e["kind"] == "text.delta" && e["data"]["text"] == "working" {
            break;
        }
    }
    let steered = core.call(
        "thread.steer",
        json!({ "thread": thread, "input": { "text": "go left" } }),
    );
    assert_eq!(steered["status"], "accepted", "{steered}");
    let mut after = Vec::new();
    let mut final_text = String::new();
    loop {
        let e = core.event(&topic, deadline);
        let kind = e["kind"].as_str().unwrap().to_owned();
        if kind == "text.done" {
            final_text = e["data"]["text"].as_str().unwrap_or_default().to_owned();
        }
        after.push(kind.clone());
        if kind == "turn.ended" {
            assert_eq!(e["data"]["outcome"], "done", "{e}");
            break;
        }
    }
    assert_eq!(
        after,
        [
            "input.steered",
            "text.delta",
            "text.done",
            "usage",
            "turn.ended"
        ]
    );
    assert_eq!(final_text, "steered ok");
}

#[test]
#[ignore = "live: needs Claude Code (`claude`) and its sign-in"]
fn live_claude() {
    let bins = Bins::locate();
    let root = repo_root();
    let claude_dir = root.join("providers/claude");
    let claude_wasm = root.join("target/wasm32-wasip2/release/kumikit_provider_claude.wasm");
    let mut core = Core::start(&bins, None);
    let claude_manifest = claude_dir.join("plugin.toml");
    let claude_schema = claude_dir.join("claude.schema.json");
    let claude_readme = claude_dir.join("README.md");
    core.install_files(
        &bins,
        "provider-claude",
        &[
            ("plugin.toml", &claude_manifest),
            ("claude.schema.json", &claude_schema),
            ("README.md", &claude_readme),
            ("plugin.wasm", &claude_wasm),
        ],
    );
    let ws = core.open_workspace();
    let topic = core.send(
        &ws,
        "kumikit.provider-claude/claude",
        "Reply with exactly the words: hello from kumikit. Do not use any tools.",
    );
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut text = String::new();
    loop {
        let e = core.event(&topic, deadline);
        match e["kind"].as_str() {
            Some("text.done") => text = e["data"]["text"].as_str().unwrap_or_default().to_owned(),
            Some("turn.ended") => {
                assert_eq!(e["data"]["outcome"], "done", "{e}");
                break;
            }
            _ => {}
        }
    }
    assert!(
        text.to_lowercase().contains("hello from kumikit"),
        "claude said {text:?}"
    );
}
