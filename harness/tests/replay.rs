//! Every provider's recordings replay through provider-acp and give the events in
//! `tests/expected/<name>.json` (`KUMIKIT_BLESS=1` rewrites them). Every provider has at
//! least one recording.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use kumikit_agents_harness::{providers, replay, transcripts};

#[test]
fn recordings_replay_to_their_expected_events() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut problems = Vec::new();
    for p in providers(root).unwrap() {
        let list = transcripts(&p);
        if list.is_empty() {
            problems.push(format!("{}: no recordings in tests/transcripts", p.name));
        }
        for (name, path) in list {
            let log = match replay(&path, p.mode.as_deref(), p.auth_method.as_deref()) {
                Ok(l) => l,
                Err(e) => {
                    problems.push(e);
                    continue;
                }
            };
            let expected = p.dir.join("tests/expected").join(format!("{name}.json"));
            let actual = serde_json::to_string_pretty(&log).unwrap() + "\n";
            if std::env::var_os("KUMIKIT_BLESS").is_some() {
                std::fs::write(&expected, &actual).unwrap();
            } else if std::fs::read_to_string(&expected).ok().as_deref() != Some(actual.as_str()) {
                problems.push(format!(
                    "{}/{name}: events differ from {} (KUMIKIT_BLESS=1 to accept)",
                    p.name,
                    expected.display()
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
