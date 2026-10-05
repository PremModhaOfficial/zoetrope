//! Stage-6 scenario tests that need the whole provider around them.
//!
//! The scenarios with a narrower subject live in the module they bind to
//! (`wire.rs`, `discovery.rs`, `mod.rs`); this file carries the end-to-end
//! counterpart of the FIN smoke: an inspect-equivalent fold of the shipped
//! fixture, and one appended line pushed through a long-lived stream the way
//! the tailer reads a growing file.

use crate::fact::{FactKind, Outcome};
use crate::provider::commandcode::Stream;
use crate::state::SessionInfo;
use crate::state::session::{AgentStatus, SessionModel};

// S-017
#[test]
fn s_017_fin_smoke_inspect_and_live_append() {
    let Some(dir) = crate::provider::harness::fixture_dir("commandcode") else {
        return;
    };
    let demo = dir.join("demo.jsonl");
    let text = std::fs::read_to_string(&demo).expect("the fixture reads");

    // Half one: the `inspect` path. Open the fixture by path, fold every
    // line through one `stream_for` stream into the model and the session
    // info, settle it, and render the report a user reads.
    let session = crate::provider::open(&crate::provider::Target::Path(demo.clone()), None)
        .expect("the fixture opens");
    assert_eq!(session.provider, crate::provider::Provider::CommandCode);
    assert_eq!(session.id, "demo");
    let mut stream = session.provider.stream_for(&session.root);
    let mut model = SessionModel::new(session.id.clone());
    let mut info = SessionInfo::default();
    for line in text.lines() {
        if let Some(statement) = stream.push(line) {
            for fact in &statement.facts {
                info.apply(fact);
                model.apply_fact(fact);
            }
        }
    }
    model.recompute_group_status();
    model.end_of_stream();
    let report = crate::state::render::report(&model, &info);
    assert!(
        report.starts_with("session demo — Exporter dry-run flag"),
        "the report begins with the titled header:\n{report}"
    );
    assert!(report.contains("2 agent(s), 2 tool call(s)"), "\n{report}");
    assert!(report.contains("[main] commandcode"), "\n{report}");
    let child_line = report
        .lines()
        .find(|line| line.contains("[subagent] Verify exporter tests pass"))
        .unwrap_or_else(|| panic!("no child line in:\n{report}"));
    assert!(child_line.contains("(done)"), "child line: {child_line}");

    // Half two: the tailer's read path. One stream across pushes — it keeps
    // its header cwd and its spawn set — then the spawn's tool_result as a
    // later append still completes the call and the child via the core join.
    let lines: Vec<&str> = text.lines().collect();
    assert!(
        lines.len() >= 6,
        "the fixture states the spawn result at line 6"
    );
    let mut stream = Stream::new();
    let mut model = SessionModel::new("demo".into());
    for line in &lines[..5] {
        if let Some(statement) = stream.push(line) {
            for fact in &statement.facts {
                model.apply_fact(fact);
            }
        }
    }
    let appended = stream.push(lines[5]).expect("the appended result states");
    assert_eq!(
        appended
            .facts
            .iter()
            .map(|f| f.kind.name())
            .collect::<Vec<_>>(),
        ["ToolEnd"]
    );
    assert!(matches!(
        &appended.facts[0].kind,
        FactKind::ToolEnd { call, outcome: Outcome::Ok } if call == "call_02_agent"
    ));
    for fact in &appended.facts {
        model.apply_fact(fact);
    }
    model.recompute_group_status();
    let child = model
        .agent("call_02_agent")
        .expect("the child was born before the append");
    assert_eq!(child.status, AgentStatus::Done);
}
