//! The jcode provider: a rewritten session document in, [`Fact`]s out.
//!
//! One file is one session, and every message in it is *by* the root agent —
//! jcode keeps no subagent transcripts. Spawned swarm workers run in sibling
//! session files (joined only by a `target_session` name in the tool input),
//! so this provider states no `Agent` children: a `swarm` spawn is still
//! marked with a [`FactKind::Spawn`](crate::fact::FactKind::Spawn) so the
//! provenance is kept, but no child is declared.
//!
//! What this module knows that the model must not: that a user record with
//! `display_role: "system"` is injected session context rather than a prompt
//! (only a person-typed prompt is an era boundary); that a `background_task`
//! notice is observed output rather than a prompt; that a missing `is_error`
//! means success; and how to render each tool's input as a one-line summary.

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub mod discovery;
pub mod wire;

use crate::fact::{Fact, FactKind, Outcome, Statement};
use crate::provider::summary::{short_path, truncate_summary};
use crate::state::session::MAIN_ID;
use wire::{Block, Document, Message, parse_doc};

/// One session document being read: the message ids already stated. Each
/// rewrite of the file is diffed against them, so only new messages state
/// facts — which is what makes replay, live tailing and `inspect` share this
/// code: every read is just "the document as of now".
#[derive(Debug, Clone, Default)]
pub struct Stream {
    seen: HashSet<String>,
    /// Whether the root agent (and the session rows) have been stated.
    announced: bool,
    /// For relativising paths in summaries.
    cwd: Option<String>,
}

impl Stream {
    pub fn new() -> Self {
        Stream::default()
    }

    /// A document provider has no line records: every line of the file is a
    /// fragment of one JSON document. Feeders read these files whole through
    /// [`push_doc`](Self::push_doc); this exists only for the uniform surface.
    pub fn push(&mut self, _line: &str) -> Option<Statement> {
        None
    }

    /// State what the document says that it did not say before. `None` while
    /// the text does not parse (a torn mid-write read — retried on the next
    /// tick); `Some` (possibly empty) once it does.
    pub fn push_doc(&mut self, text: &str) -> Option<Vec<Statement>> {
        let doc = parse_doc(text)?;
        let mut out = Vec::new();
        if !self.announced {
            self.announced = true;
            self.cwd = doc.working_dir.clone();
            out.push(root_statement(&doc));
        }
        for msg in doc.messages.iter() {
            // The seen key is the id (when present) PLUS a hash of the
            // message's canonical JSON. An id reused with different content
            // (an edit) states again instead of being swallowed as a
            // duplicate; an id-less record keys by content, not position, so
            // an insert in the middle does not shift every later key and
            // re-state the tail.
            let key = match &msg.id {
                Some(id) => format!("{id}:{:016x}", message_hash(msg)),
                None => format!("message:{:016x}", message_hash(msg)),
            };
            if !self.seen.insert(key) {
                continue;
            }
            let facts = message_facts(msg, self.cwd.as_deref());
            if facts.is_empty() {
                continue;
            }
            out.push(Statement {
                at: msg.timestamp,
                facts,
            });
        }
        Some(out)
    }
}

/// The root agent, stated once per stream, with the session rows the document
/// carries. Untimed session metadata, off the timeline.
fn root_statement(doc: &Document) -> Statement {
    let mut facts = vec![Fact {
        agent: Some(MAIN_ID.to_string()),
        ts: None,
        kind: FactKind::Agent {
            kind: crate::fact::AgentKind::Main,
            parent: None,
            agent_type: Some("jcode".into()),
            description: None,
            spawned_by: None,
            interactive: true,
        },
    }];
    let session = |label: &str, value: &Option<String>| {
        value.as_ref().map(|v| Fact {
            agent: None,
            ts: None,
            kind: FactKind::Session {
                label: label.into(),
                value: v.clone(),
            },
        })
    };
    facts.extend(session("cwd", &doc.working_dir));
    facts.extend(session("model", &doc.model));
    facts.extend(session("provider", &doc.provider_key));
    if let Some(title) = doc.title.as_deref().filter(|t| !t.trim().is_empty()) {
        facts.push(Fact {
            agent: None,
            ts: None,
            kind: FactKind::Title(title.to_string()),
        });
    }
    Statement { at: None, facts }
}

/// Facts stated by one message of the document, all about the root agent.
///
/// Record → fact mapping (the whole provider contract in one table):
///
/// | record | states |
/// |---|---|
/// | assistant `text`/`reasoning` (non-blank) | `Reasoning` |
/// | assistant `tool_use` (with call id) | `ToolStart` (+ `Spawn` iff `swarm`/`spawn`) |
/// | user `tool_result` | `ToolEnd` (`Ok` unless `is_error: true`) |
/// | user `text`, no `display_role` | `Prompt` |
/// | user `text`, `display_role: system` | nothing (injected context, not a prompt) |
/// | user `text`, `display_role: background_task` | `Reasoning` (observed output) |
/// | anything else (blank, unknown role/block) | `Activity`, or nothing for unknown roles |
fn message_facts(msg: &Message, cwd: Option<&str>) -> Vec<Fact> {
    let mut out = Vec::new();
    let by_main = |kind| Fact {
        agent: Some(MAIN_ID.to_string()),
        ts: None,
        kind,
    };
    match msg.role.as_deref() {
        Some("assistant") => {
            for block in &msg.content {
                match block {
                    Block::Text { text } | Block::Reasoning { text } if !text.trim().is_empty() => {
                        out.push(by_main(FactKind::Reasoning(text.clone())));
                    }
                    Block::ToolUse(tu) => {
                        let Some(call) = tu.id.clone() else {
                            continue;
                        };
                        let name = tu.name.clone().unwrap_or_default();
                        let summary = summarize_tool(&name, &tu.input, cwd);
                        out.push(by_main(FactKind::ToolStart {
                            call: call.clone(),
                            name: name.clone(),
                            summary,
                        }));
                        if is_spawn_tool(&name, &tu.input) {
                            out.push(by_main(FactKind::Spawn { call }));
                        }
                    }
                    _ => {}
                }
            }
        }
        Some("user") => {
            let system = msg.display_role.as_deref() == Some("system");
            let background = msg.display_role.as_deref() == Some("background_task");
            for block in &msg.content {
                match block {
                    Block::ToolResult(tr) => {
                        let Some(call) = tr.tool_use_id.clone() else {
                            continue;
                        };
                        out.push(by_main(FactKind::ToolEnd {
                            call,
                            outcome: if tr.is_error == Some(true) {
                                Outcome::Err
                            } else {
                                Outcome::Ok
                            },
                        }));
                    }
                    Block::Text { text } if !text.trim().is_empty() => {
                        if system {
                            // Injected session context: nobody said it, so it
                            // is activity at most (counted below), never a
                            // prompt or reasoning.
                        } else if background {
                            out.push(by_main(FactKind::Reasoning(text.clone())));
                        } else {
                            out.push(by_main(FactKind::Prompt(text.clone())));
                        }
                    }
                    _ => {}
                }
            }
        }
        // An unknown role states nothing rather than attributing its text to
        // the wrong agent.
        _ => return Vec::new(),
    }
    if !out.iter().any(|f| f.agent.as_deref() == Some(MAIN_ID)) {
        out.push(by_main(FactKind::Activity));
    }
    out
}

/// Content hash of one message's canonical JSON (struct field order, so
/// stable across rewrites). Feeds the [`Stream`] seen key: equal content
/// hashes equal keys, so a steady rewrite states nothing, while any edit
/// re-states.
fn message_hash(msg: &Message) -> u64 {
    let mut h = DefaultHasher::new();
    // Serialization of an in-memory message cannot fail in practice; a
    // fallback empty hash only ever collides with another fallback.
    serde_json::to_string(msg).unwrap_or_default().hash(&mut h);
    h.finish()
}

/// Which tool calls spawn an agent: a `swarm` spawn. Every other swarm action
/// (status, report, plan bookkeeping, dm/message to a sibling session) and
/// every `bg` action (wait, output, status, cancel) only observes workers
/// that run in sibling sessions.
///
/// Census 2026-10-05 over all 391 live `~/.jcode/sessions/session_*.json`:
/// tool names are bash/swarm/read/webfetch/todo/agentgrep/batch/edit/bg/…
/// — no tool named spawn/task/agent exists — and within `swarm` only
/// `action == "spawn"` (50 hits, each carrying a `label`+`prompt`) creates a
/// worker. Pinned by `only_swarm_spawn_marks_provenance` below.
fn is_spawn_tool(name: &str, input: &serde_json::Value) -> bool {
    name == "swarm" && input.get("action").and_then(|a| a.as_str()) == Some("spawn")
}

// ---------------------------------------------------------------------------
// Tool summaries: the jcode tool vocabulary and what makes a good one-liner
// ---------------------------------------------------------------------------

/// Derive a short one-line summary from a tool_use input, if a natural field
/// exists for the tool. Defensive: any shape that doesn't match yields `None`.
pub(crate) fn summarize_tool(
    name: &str,
    input: &serde_json::Value,
    cwd: Option<&str>,
) -> Option<String> {
    let pick = |key: &str| {
        input
            .get(key)
            .and_then(|v| v.as_str())
            .map(truncate_summary)
    };
    let pick_path = |key: &str| {
        input
            .get(key)
            .and_then(|v| v.as_str())
            .map(|p| short_path(p, cwd))
    };
    match name {
        "bash" => pick("command"),
        "read" | "write" | "edit" => pick_path("file_path").or_else(|| pick_path("path")),
        "ls" => pick_path("path"),
        "webfetch" => pick("url"),
        "websearch" | "agentgrep" => pick("query"),
        "swarm" | "bg" | "todo" => pick("intent").or_else(|| pick("action")),
        _ => pick("command")
            .or_else(|| pick_path("file_path"))
            .or_else(|| pick_path("path"))
            .or_else(|| pick("query"))
            .or_else(|| pick("url"))
            .or_else(|| pick("intent"))
            .or_else(|| pick("description")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::AgentKind;

    /// Push a document through a fresh stream, stating everything in it.
    fn statements(text: &str) -> Vec<Statement> {
        Stream::new().push_doc(text).expect("parses")
    }

    fn kinds(st: &Statement) -> Vec<&str> {
        st.facts.iter().map(|f| f.kind.name()).collect()
    }

    #[test]
    fn root_is_announced_once_with_session_rows() {
        let mut s = Stream::new();
        let doc = r#"{"id": "s", "working_dir": "/project", "model": "m", "provider_key": "p",
            "messages": [{"id": "m1", "role": "user",
            "timestamp": "2026-09-01T10:00:00.000Z",
            "content": [{"type": "text", "text": "hi"}]}]}"#;
        let first = s.push_doc(doc).expect("parses");
        assert!(matches!(
            &first[0].facts[0].kind,
            FactKind::Agent { kind: AgentKind::Main, agent_type, interactive: true, .. }
                if agent_type.as_deref() == Some("jcode")
        ));
        assert_eq!(first[0].facts[0].agent.as_deref(), Some("main"));
        assert!(
            first[0]
                .facts
                .iter()
                .filter(|f| f.is_session_meta())
                .count()
                >= 3,
            "cwd + model + provider rows"
        );
        // A rewrite with nothing new states nothing — and no second root.
        let again = s.push_doc(doc).expect("parses");
        assert!(again.is_empty(), "a steady document states nothing");
    }

    #[test]
    fn only_new_messages_state_facts() {
        let mut s = Stream::new();
        let first = s
            .push_doc(
                r#"{"id": "s", "working_dir": "/p", "messages": [
                    {"id": "m1", "role": "user", "timestamp": "2026-09-01T10:00:00.000Z",
                     "content": [{"type": "text", "text": "first"}]}]}"#,
            )
            .expect("parses");
        assert_eq!(first.len(), 2, "root + first message");
        // One turn later: the rewrite carries both, only the second states.
        let second = s
            .push_doc(
                r#"{"id": "s", "working_dir": "/p", "messages": [
                    {"id": "m1", "role": "user", "timestamp": "2026-09-01T10:00:00.000Z",
                     "content": [{"type": "text", "text": "first"}]},
                    {"id": "m2", "role": "assistant", "timestamp": "2026-09-01T10:00:05.000Z",
                     "content": [{"type": "text", "text": "on it"}]}]}"#,
            )
            .expect("parses");
        assert_eq!(second.len(), 1);
        assert!(matches!(
            &second[0].facts[0].kind,
            FactKind::Reasoning(t) if t == "on it"
        ));
        assert_eq!(second[0].at, second[0].facts[0].ts.or(second[0].at));
    }

    #[test]
    fn torn_write_states_nothing_and_keeps_state() {
        let mut s = Stream::new();
        assert!(s.push_doc(r#"{"id": "s", "messages": [{"id": "#).is_none());
        assert!(!s.announced, "a failed parse announces nothing");
        // The completed write then states everything, from the root on.
        let done = s
            .push_doc(
                r#"{"id": "s", "messages": [{"id": "m1", "role": "user",
                "content": [{"type": "text", "text": "hi"}]}]}"#,
            )
            .expect("parses");
        assert_eq!(done.len(), 2, "root + message");
    }

    #[test]
    fn prompts_tool_calls_and_outcomes() {
        let sts = statements(
            r#"{"id": "s", "messages": [
                {"id": "m1", "role": "assistant", "timestamp": "2026-09-01T10:00:00.000Z",
                 "content": [{"type": "reasoning", "text": "hmm"},
                             {"type": "tool_use", "id": "call_1", "name": "bash",
                              "input": {"command": "cargo test", "intent": "run tests"}}]},
                {"id": "m2", "role": "user", "timestamp": "2026-09-01T10:00:01.000Z",
                 "content": [{"type": "tool_result", "tool_use_id": "call_1",
                              "content": "ok", "is_error": false}]},
                {"id": "m3", "role": "user", "timestamp": "2026-09-01T10:00:02.000Z",
                 "content": [{"type": "tool_result", "tool_use_id": "call_9",
                              "content": "blocked"}]}
            ]}"#,
        );
        // [root, assistant, ok-result, missing-flag result]
        assert_eq!(sts.len(), 4);
        assert_eq!(kinds(&sts[1]), ["Reasoning", "ToolStart"]);
        assert!(matches!(&sts[1].facts[1].kind,
                FactKind::ToolStart { call, name, summary }
                if call == "call_1" && name == "bash" && summary.as_deref() == Some("cargo test")));
        assert!(matches!(
            &sts[2].facts[0].kind,
            FactKind::ToolEnd {
                outcome: Outcome::Ok,
                ..
            }
        ));
        // A missing `is_error` is success, like Claude's format.
        assert!(matches!(
            &sts[3].facts[0].kind,
            FactKind::ToolEnd {
                outcome: Outcome::Ok,
                ..
            }
        ));
    }

    #[test]
    fn system_context_is_never_a_prompt() {
        let sts = statements(
            r#"{"id": "s", "messages": [
                {"id": "m1", "role": "user", "display_role": "system",
                 "timestamp": "2026-09-01T10:00:00.000Z",
                 "content": [{"type": "text", "text": "<system-reminder>\nDate: today</system-reminder>"}]},
                {"id": "m2", "role": "user", "display_role": "background_task",
                 "timestamp": "2026-09-01T10:00:01.000Z",
                 "content": [{"type": "text", "text": "worker done"}]},
                {"id": "m3", "role": "user", "timestamp": "2026-09-01T10:00:02.000Z",
                 "content": [{"type": "text", "text": "real prompt"}]}
            ]}"#,
        );
        assert_eq!(kinds(&sts[1]), ["Activity"]);
        assert_eq!(kinds(&sts[2]), ["Reasoning"]);
        assert_eq!(kinds(&sts[3]), ["Prompt"]);
    }

    #[test]
    fn swarm_spawn_marks_provenance_but_declares_no_child() {
        let sts = statements(
            r#"{"id": "s", "messages": [
                {"id": "m1", "role": "assistant", "timestamp": "2026-09-01T10:00:00.000Z",
                 "content": [{"type": "tool_use", "id": "call_s", "name": "swarm",
                              "input": {"action": "spawn", "intent": "verify the merge"}}]},
                {"id": "m2", "role": "assistant", "timestamp": "2026-09-01T10:00:01.000Z",
                 "content": [{"type": "tool_use", "id": "call_w", "name": "swarm",
                              "input": {"action": "status", "intent": "check workers"}}]}
            ]}"#,
        );
        assert!(
            sts[1]
                .facts
                .iter()
                .any(|f| matches!(&f.kind, FactKind::Spawn { call } if call == "call_s")),
            "a spawn marks provenance"
        );
        assert!(
            !sts[2]
                .facts
                .iter()
                .any(|f| matches!(&f.kind, FactKind::Spawn { .. })),
            "a status check is not a spawn"
        );
        assert!(
            sts.iter().flat_map(|st| &st.facts).all(|f| !matches!(
                &f.kind,
                FactKind::Agent {
                    kind: AgentKind::Subagent,
                    ..
                }
            )),
            "no children are ever declared: workers live in sibling sessions"
        );
    }

    #[test]
    fn only_swarm_spawn_marks_provenance() {
        let v = |s: &str| serde_json::from_str(s).unwrap();
        assert!(is_spawn_tool("swarm", &v(r#"{"action":"spawn"}"#)));
        // Census 2026-10-05: no other tool name or action spawns. `bg`
        // observes background tasks; `swarm` dm/message/report/plan actions
        // talk to (not create) sibling sessions; nothing is named
        // spawn/task/agent at all.
        for (name, input) in [
            ("swarm", r#"{"action":"status"}"#),
            ("swarm", r#"{"action":"report"}"#),
            ("swarm", r#"{"action":"complete_node"}"#),
            ("swarm", r#"{"action":"expand_node"}"#),
            ("swarm", r#"{"action":"task_graph"}"#),
            ("swarm", r#"{"action":"dm","target_session":"session_x"}"#),
            (
                "swarm",
                r#"{"action":"message","target_session":"session_x"}"#,
            ),
            ("swarm", r#"{}"#),
            ("bg", r#"{"action":"wait"}"#),
            ("bg", r#"{"action":"output"}"#),
            ("bg", r#"{"action":"status"}"#),
            ("bg", r#"{"action":"cancel"}"#),
            ("bg", r#"{"action":"spawn"}"#),
            ("task", r#"{"action":"spawn"}"#),
            ("agent", r#"{}"#),
            ("spawn", r#"{}"#),
            ("bash", r#"{"command":"cargo test"}"#),
        ] {
            assert!(
                !is_spawn_tool(name, &v(input)),
                "{name} {input} must not mark provenance"
            );
        }
    }

    #[test]
    fn same_id_with_edited_content_states_again() {
        let mut s = Stream::new();
        let doc = |text: &str| {
            format!(
                r#"{{"id": "s", "messages": [
                    {{"id": "m1", "role": "user", "timestamp": "2026-09-01T10:00:00.000Z",
                      "content": [{{"type": "text", "text": "{text}"}}]}}]}}"#
            )
        };
        let first = s.push_doc(&doc("first")).expect("parses");
        assert_eq!(first.len(), 2, "root + message");
        // The same id carrying edited text is an edit, not a duplicate.
        let edited = s.push_doc(&doc("first, revised")).expect("parses");
        assert_eq!(edited.len(), 1);
        assert!(matches!(&edited[0].facts[0].kind, FactKind::Prompt(t) if t == "first, revised"));
        // The same id with the same content is steady: nothing.
        assert!(
            s.push_doc(&doc("first, revised"))
                .expect("parses")
                .is_empty()
        );
    }

    #[test]
    fn id_less_messages_key_by_content_not_position() {
        let mut s = Stream::new();
        let doc = |texts: &[&str]| {
            let msgs: Vec<String> = texts
                .iter()
                .map(|t| {
                    format!(
                        r#"{{"role": "user", "timestamp": "2026-09-01T10:00:00.000Z",
                            "content": [{{"type": "text", "text": "{t}"}}]}}"#
                    )
                })
                .collect();
            format!(r#"{{"id": "s", "messages": [{}]}}"#, msgs.join(","))
        };
        let first = s.push_doc(&doc(&["alpha", "beta"])).expect("parses");
        assert_eq!(first.len(), 3, "root + two id-less messages");
        // An insert in the middle states only the newcomer: position-keyed
        // fallbacks would shift "beta" and re-state it as a duplicate.
        let inserted = s.push_doc(&doc(&["alpha", "NEW", "beta"])).expect("parses");
        assert_eq!(inserted.len(), 1);
        assert!(matches!(&inserted[0].facts[0].kind, FactKind::Prompt(t) if t == "NEW"));
    }

    #[test]
    fn summarize_tool_picks_the_natural_field() {
        let input = |v: serde_json::Value| v;
        assert_eq!(
            summarize_tool(
                "bash",
                &input(serde_json::json!({"command": "cargo test", "intent": "t"})),
                Some("/p")
            )
            .as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            summarize_tool(
                "read",
                &input(serde_json::json!({"file_path": "/p/src/main.rs"})),
                Some("/p")
            )
            .as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            summarize_tool(
                "swarm",
                &input(serde_json::json!({"action": "spawn", "intent": "verify it"})),
                None
            )
            .as_deref(),
            Some("verify it")
        );
        assert_eq!(
            summarize_tool("unknown-tool", &input(serde_json::json!({})), None),
            None
        );
    }

    /// The shipped demo session, read the way a live one is (whole-document
    /// diffs through one stream) and run through the whole conformance check:
    /// order invariance plus the model and timeline goldens beside the
    /// fixture. The single stream makes order invariance trivial — which is
    /// the point: a document cannot interleave with itself.
    #[test]
    fn demo_session_conforms() {
        let Some(dir) = crate::provider::harness::fixture_dir("jcode") else {
            return;
        };
        let path = dir.join("session_demo_1700000000000_aaaaaaaaaaaaaaaa.json");
        let streams = || {
            let text = std::fs::read_to_string(&path).unwrap();
            let mut stream = Stream::new();
            vec![stream.push_doc(&text).unwrap_or_default()]
        };
        crate::provider::harness::conform("jcode", "demo", streams);
    }

    /// A fixture opens by path the way `zoe <file>` does: the content does
    /// not name the provider on its first line (a bare `{`), so the layout
    /// fallback claims it — and the session is its one document.
    #[test]
    fn fixture_opens_by_path() {
        let Some(dir) = crate::provider::harness::fixture_dir("jcode") else {
            return;
        };
        let path = dir.join("session_demo_1700000000000_aaaaaaaaaaaaaaaa.json");
        let s = crate::provider::open(&crate::provider::Target::Path(path.clone()), None).unwrap();
        assert_eq!(s.provider, crate::provider::Provider::Jcode);
        assert_eq!(s.id, "session_demo_1700000000000_aaaaaaaaaaaaaaaa");
        assert_eq!(s.root.path, path);
        assert_eq!(s.root.read, crate::provider::ReadMode::Doc);
        assert!(s.files.is_empty(), "a session is its one document");
    }
}
