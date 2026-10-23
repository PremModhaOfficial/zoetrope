//! The Command Code provider: one JSONL line in, [`Fact`]s out.
//!
//! Sessions are single-file: the header, messages, model and effort changes,
//! compactions and the session title all live in one append-only
//! `<uuid>.jsonl` under `~/.commandcode/projects/<slug>/`. A spawned agent's
//! turn is not a file of its own (children are in-memory only), so the child
//! exists in the parent's transcript as the `agent` tool_use / tool_result
//! pair, keyed by the call id, and its completion is the core's own
//! acknowledgement join — never a fact this provider states.
//!
//! What this module knows that the model must not: that only `user`,
//! `steering` and `followup` sources are human prompts; that `agent` spawns
//! while `agent_output` only polls; that a `tool_result` without `is_error`
//! is a success; that `compaction` is a tally, `session_info` carries the
//! title and `effort_change` is session state (not agent naming); and how to
//! render each tool's input as a one-line summary.

use chrono::{DateTime, Utc};

pub mod discovery;
pub mod wire;

#[cfg(test)]
mod scenarios;

use crate::fact::{AgentKind, Fact, FactKind, Outcome, Statement};
use crate::provider::summary::{short_path, truncate_summary};
use crate::state::session::MAIN_ID;
use wire::{ContentBlock, Entry, MessageEntry, ToolUse};

/// One session file being read. The header feeds two pieces of state: the
/// cwd (for relativising tool paths in summaries) and whether the root has
/// been announced. Effort is tracked so an unchanged value is not restated,
/// and the spawn set joins a spawn call to its acknowledgement.
#[derive(Debug, Clone, Default)]
pub struct Stream {
    /// cwd from the header, for relativising tool paths in summaries.
    cwd: Option<String>,
    /// Whether the header has stated the root. A repeated header states
    /// nothing — `announced` gates the whole header statement (`Agent` + both
    /// `Session` rows), so a second header push yields `None`.
    announced: bool,
    /// Effort last stated, so only the first value and each change is restated.
    effort: Option<String>,
    /// Spawn calls seen here, the join for the spawn call's tool_result.
    spawned: std::collections::HashSet<String>,
}

impl Stream {
    pub fn new() -> Self {
        Stream::default()
    }

    /// Parse one line and state what it says. `None` for a blank or
    /// unparsable line, or one that states nothing.
    pub fn push(&mut self, line: &str) -> Option<Statement> {
        let entry = wire::parse_line(line)?;
        self.push_entry(&entry)
    }

    /// State what an already-parsed entry says.
    pub fn push_entry(&mut self, entry: &Entry) -> Option<Statement> {
        let at = entry_timestamp(entry);
        let mut facts = match entry {
            Entry::Session(header) => {
                if self.announced {
                    return None;
                }
                let version = header.version_number()?;
                self.announced = true;
                let mut facts = vec![main_fact(
                    FactKind::Agent {
                        kind: AgentKind::Main,
                        parent: None,
                        agent_type: Some("commandcode".into()),
                        description: None,
                        spawned_by: None,
                        interactive: true,
                    },
                    at,
                )];
                if let Some(cwd) = &header.cwd {
                    self.cwd = Some(cwd.clone());
                    facts.push(meta(FactKind::Session {
                        label: "cwd".into(),
                        value: cwd.clone(),
                    }));
                }
                facts.push(meta(FactKind::Session {
                    label: "version".into(),
                    value: version.to_string(),
                }));
                facts
            }
            Entry::Message(message) => self.message_facts(message, at),
            Entry::Compaction(_) => vec![meta(FactKind::Tally("compaction".into()))],
            Entry::ModelChange(change) => change
                .model
                .as_ref()
                .map(|model| main_fact(FactKind::Model(model.clone()), at))
                .into_iter()
                .collect(),
            Entry::EffortChange(change) => self.effort_facts(change.effort.as_deref()),
            Entry::SessionInfo(info) => match info.name.as_deref() {
                Some(name) if !name.trim().is_empty() => {
                    vec![meta(FactKind::Title(name.to_string()))]
                }
                _ => Vec::new(),
            },
            Entry::Unknown => Vec::new(),
        };
        if facts.is_empty() {
            return None;
        }
        for fact in &mut facts {
            if fact.ts.is_none() {
                fact.ts = at;
            }
        }
        Some(Statement { at, facts })
    }

    /// Facts stated by one message entry. Ordering matters: effort is
    /// observed first, then the model and token delta, then the content
    /// blocks in file order, then the activity fallback.
    fn message_facts(&mut self, entry: &MessageEntry, at: Option<DateTime<Utc>>) -> Vec<Fact> {
        let mut out = Vec::new();
        let role = entry.message.as_ref().and_then(|m| m.role.as_deref());
        // The per-message effort field observes effort like `effort_change`
        // does; 8743 real entries carry it, and a session that never changes
        // effort would otherwise show no effort row at all.
        if let Some(effort) = entry.effort.as_deref() {
            self.observe_effort(effort, &mut out);
        }
        if role == Some("assistant") {
            if let Some(model) = &entry.model {
                out.push(main_fact(FactKind::Model(model.clone()), at));
            }
            if let Some(output) = entry.usage.as_ref().and_then(|u| u.output_tokens) {
                // The `messageId` is unique per entry, so a re-applied or
                // re-read entry is a fold no-op; a missing one degrades to
                // the per-entry summing path.
                let dedup = entry
                    .message
                    .as_ref()
                    .and_then(|m| m.meta.as_ref())
                    .and_then(|meta| meta.message_id.clone());
                out.push(main_fact(FactKind::Tokens { output, dedup }, at));
            }
            if let Some(message) = &entry.message {
                for block in &message.content {
                    match block {
                        ContentBlock::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                            out.push(main_fact(FactKind::Reasoning(thinking.clone()), at));
                        }
                        ContentBlock::Text { text } if !text.trim().is_empty() => {
                            out.push(main_fact(FactKind::Reasoning(text.clone()), at));
                        }
                        ContentBlock::ToolUse(tool_use) => {
                            self.tool_use_facts(tool_use, &mut out, at);
                        }
                        _ => {}
                    }
                }
            }
        }
        if role == Some("user") {
            self.user_facts(entry, &mut out, at);
        }
        // An entry that states no fact about main is still its activity.
        if !out.iter().any(|f| f.agent.as_deref() == Some(MAIN_ID)) {
            out.push(main_fact(FactKind::Activity, at));
        }
        out
    }

    /// Facts stated by a user message: the prompt gate, then every result
    /// block. Result blocks map whatever the source says — rows 10/11 run on
    /// `tool`, absent and `scheduled-cron:<id>` entries alike.
    fn user_facts(&mut self, entry: &MessageEntry, out: &mut Vec<Fact>, at: Option<DateTime<Utc>>) {
        let Some(message) = &entry.message else {
            return;
        };
        let source = message
            .meta
            .as_ref()
            .and_then(|meta| meta.source.as_deref());
        // Only a human source prompts: `user`, `steering`, `followup`.
        // Absent, `tool`, `model`, `scheduled-cron:<id>` and every unknown
        // value never do (`SOURCE-DEFAULT`).
        if matches!(source, Some("user" | "steering" | "followup")) {
            let text: String = message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.trim().is_empty() {
                out.push(main_fact(FactKind::Prompt(text), at));
            }
        }
        for block in &message.content {
            let ContentBlock::ToolResult(result) = block else {
                continue;
            };
            let Some(call) = &result.tool_use_id else {
                continue;
            };
            let outcome = if result.is_error == Some(true) {
                Outcome::Err
            } else {
                Outcome::Ok
            };
            // Rows 10/11: a spawn-matched call's result is its
            // acknowledgement and states nothing beyond this `ToolEnd` — the
            // child's completion is the core's join (`SPAWN`), never a
            // provider fact — and any other call's result is the same end.
            let _is_spawn_ack = self.spawned.contains(call);
            out.push(main_fact(
                FactKind::ToolEnd {
                    call: call.clone(),
                    outcome,
                },
                at,
            ));
        }
    }

    /// Facts stated by an assistant `tool_use`. A call with an id is a
    /// `ToolStart`; the `agent` tool additionally spawns a child keyed by
    /// that call, whose completion the core joins from the call's own
    /// `tool_result`.
    fn tool_use_facts(
        &mut self,
        tool_use: &ToolUse,
        out: &mut Vec<Fact>,
        at: Option<DateTime<Utc>>,
    ) {
        let Some(call) = &tool_use.id else {
            return;
        };
        let name = tool_use.name.clone().unwrap_or_default();
        let summary = summarize_tool(&name, &tool_use.input, self.cwd.as_deref());
        out.push(main_fact(
            FactKind::ToolStart {
                call: call.clone(),
                name: name.clone(),
                summary,
            },
            at,
        ));
        if !is_spawn_tool(&name) {
            return;
        }
        self.spawned.insert(call.clone());
        out.push(main_fact(FactKind::Spawn { call: call.clone() }, at));
        out.push(Fact {
            agent: Some(call.clone()),
            ts: at,
            kind: FactKind::Agent {
                kind: AgentKind::Subagent,
                parent: Some(MAIN_ID.to_string()),
                agent_type: None,
                description: tool_use
                    .input
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                spawned_by: Some(call.clone()),
                interactive: false,
            },
        });
    }

    /// One `Session` row for the first observed effort value and each change
    /// after it; an unchanged repeat states nothing.
    fn observe_effort(&mut self, effort: &str, out: &mut Vec<Fact>) {
        if self.effort.as_deref() == Some(effort) {
            return;
        }
        self.effort = Some(effort.to_string());
        out.push(meta(FactKind::Session {
            label: "effort".into(),
            value: effort.to_string(),
        }));
    }

    /// Facts stated by an `effort_change` entry, if it names an effort.
    fn effort_facts(&mut self, effort: Option<&str>) -> Vec<Fact> {
        let mut out = Vec::new();
        if let Some(effort) = effort {
            self.observe_effort(effort, &mut out);
        }
        out
    }
}

/// Whether a tool name spawns an agent. Exactly one tool, and
/// `agent_output` polls rather than spawns.
pub fn is_spawn_tool(name: &str) -> bool {
    name == "agent"
}

/// The envelope timestamp of an entry, if it carries one.
fn entry_timestamp(entry: &Entry) -> Option<DateTime<Utc>> {
    match entry {
        Entry::Session(header) => header.timestamp,
        Entry::Message(message) => message.envelope.timestamp,
        Entry::Compaction(entry) => entry.envelope.timestamp,
        Entry::ModelChange(entry) => entry.envelope.timestamp,
        Entry::EffortChange(entry) => entry.envelope.timestamp,
        Entry::SessionInfo(entry) => entry.envelope.timestamp,
        Entry::Unknown => None,
    }
}

/// A fact about the root agent.
fn main_fact(kind: FactKind, ts: Option<DateTime<Utc>>) -> Fact {
    Fact {
        agent: Some(MAIN_ID.to_string()),
        ts,
        kind,
    }
}

/// A session-level fact: about the session, not about any agent.
fn meta(kind: FactKind) -> Fact {
    Fact {
        agent: None,
        ts: None,
        kind,
    }
}

// ---------------------------------------------------------------------------
// Tool summaries: the Command Code tool vocabulary and what makes a good
// one-liner
// ---------------------------------------------------------------------------

/// Derive a short one-line summary from a tool_use input. Defensive: any
/// shape that matches nothing yields `None`.
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
            .map(|path| short_path(path, cwd))
    };
    match name {
        "shell_command" => pick("command").or_else(|| pick("description")),
        "read_file" | "write_file" | "edit_file" => pick_path("file_path")
            .or_else(|| pick_path("path"))
            .or_else(|| {
                input
                    .get("paths")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str())
                    .map(|path| short_path(path, cwd))
            }),
        "read_directory" => pick_path("path"),
        "grep" | "glob" => pick("pattern"),
        "web_search" => pick("query"),
        "web_fetch" => pick("url"),
        "agent" => pick("description"),
        _ => pick("description")
            .or_else(|| pick("query"))
            .or_else(|| pick("command")),
    }
}

/// The single process-wide environment guard for every test that reads or
/// writes `COMMANDCODE_HOME` (or `HOME`, which [`discovery::commandcode_home`]
/// falls back to). Edition 2024's `set_var` is `unsafe` because the process
/// environment is global: every such test takes this one lock, sets what it
/// needs, and restores it on drop. No env-touching test may run unlocked.
#[cfg(test)]
pub(crate) mod test_env {
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    /// Holds [`LOCK`] for the test's duration and restores both environment
    /// variables on drop.
    pub(crate) struct EnvGuard {
        old_commandcode_home: Option<OsString>,
        old_home: Option<OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        /// A reader's lock: no environment write, but serialized against the
        /// tests that do write.
        pub(crate) fn locked() -> MutexGuard<'static, ()> {
            LOCK.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// Take the lock and point `COMMANDCODE_HOME` at `home` for the
        /// test's duration.
        pub(crate) fn with_home(home: &Path) -> Self {
            let lock = Self::locked();
            let guard = EnvGuard {
                old_commandcode_home: std::env::var_os("COMMANDCODE_HOME"),
                old_home: std::env::var_os("HOME"),
                _lock: lock,
            };
            guard.set_commandcode_home(Some(home));
            guard
        }

        /// Set or remove `COMMANDCODE_HOME`; the guard's lock is held already.
        pub(crate) fn set_commandcode_home(&self, home: Option<&Path>) {
            // SAFETY: `self._lock` is held for as long as `self` lives, so no
            // other env-touching test observes the transient value.
            unsafe {
                match home {
                    Some(path) => std::env::set_var("COMMANDCODE_HOME", path),
                    None => std::env::remove_var("COMMANDCODE_HOME"),
                }
            }
        }

        /// Set or remove `HOME`; the guard's lock is held already.
        pub(crate) fn set_home(&self, home: Option<&Path>) {
            // SAFETY: as `set_commandcode_home`.
            unsafe {
                match home {
                    Some(path) => std::env::set_var("HOME", path),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: still holding LOCK; restoring the prior state.
            unsafe {
                match self.old_commandcode_home.take() {
                    Some(v) => std::env::set_var("COMMANDCODE_HOME", v),
                    None => std::env::remove_var("COMMANDCODE_HOME"),
                }
                match self.old_home.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SessionInfo;
    use crate::state::session::{AgentStatus, SessionModel, ToolState};

    const HEADER: &str = r#"{"type":"session","version":3,"id":"demo","timestamp":"2026-09-30T10:00:00.000Z","cwd":"/project"}"#;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn stream_at_header() -> Stream {
        let mut stream = Stream::new();
        assert!(stream.push(HEADER).is_some());
        stream
    }

    fn stated(stream: &mut Stream, line: &str) -> Vec<Fact> {
        stream
            .push(line)
            .map(|statement| statement.facts)
            .unwrap_or_default()
    }

    fn kinds(facts: &[Fact]) -> Vec<&'static str> {
        facts.iter().map(|f| f.kind.name()).collect()
    }

    #[test]
    fn header_states_root_and_session_rows_once() {
        let mut stream = Stream::new();
        let statement = stream.push(HEADER).expect("the header states");
        assert_eq!(statement.at, Some(at("2026-09-30T10:00:00Z")));
        assert_eq!(statement.facts.len(), 3);
        let agent = &statement.facts[0];
        assert_eq!(agent.agent.as_deref(), Some("main"));
        assert_eq!(agent.ts, Some(at("2026-09-30T10:00:00Z")));
        assert!(matches!(
            &agent.kind,
            FactKind::Agent {
                kind: AgentKind::Main,
                parent: None,
                agent_type: Some(t),
                description: None,
                spawned_by: None,
                interactive: true,
            } if t.as_str() == "commandcode"
        ));
        assert!(matches!(
            &statement.facts[1].kind,
            FactKind::Session { label, value } if label == "cwd" && value == "/project"
        ));
        assert_eq!(statement.facts[1].agent, None);
        assert!(statement.facts[1].is_session_meta());
        assert!(matches!(
            &statement.facts[2].kind,
            FactKind::Session { label, value } if label == "version" && value == "3"
        ));
        assert!(statement.facts[2].is_session_meta());

        // A repeated header restates nothing.
        assert!(stream.push(HEADER).is_none());

        let mut model = SessionModel::new("demo".into());
        let mut info = SessionInfo::default();
        for fact in &statement.facts {
            info.apply(fact);
            model.apply_fact(fact);
        }
        assert_eq!(
            model.agent("main").unwrap().agent_type.as_deref(),
            Some("commandcode")
        );
        assert_eq!(info.field("cwd"), Some("/project"));
        assert_eq!(info.field("version"), Some("3"));
    }

    #[test]
    fn prompt_only_for_human_sources() {
        let text = |source: &str| {
            format!(
                r#"{{"type":"message","id":"{source}","timestamp":"2026-09-30T10:00:04.000Z","message":{{"role":"user","content":[{{"type":"text","text":"hello from {source}"}}],"meta":{{"source":"{source}","messageId":"m-{source}"}}}}}}"#
            )
        };
        let mut stream = stream_at_header();
        for source in ["user", "steering", "followup"] {
            let facts = stated(&mut stream, &text(source));
            assert!(
                facts.iter().any(|f| matches!(
                    &f.kind,
                    FactKind::Prompt(t) if t == &format!("hello from {source}")
                )),
                "{source} prompts"
            );
        }
        for source in ["tool", "model"] {
            let facts = stated(&mut stream, &text(source));
            assert!(
                facts
                    .iter()
                    .all(|f| !matches!(&f.kind, FactKind::Prompt(_))),
                "{source} does not prompt"
            );
        }
        // An absent source never prompts; a `tool_result` riding in the same
        // entry still maps (rows 10/11 run regardless of source).
        let no_source = r#"{"type":"message","id":"ns","timestamp":"2026-09-30T10:00:05.000Z","message":{"role":"user","content":[{"type":"text","text":"Error: 503 Service Unavailable"},{"type":"tool_result","tool_use_id":"call_01_edit","content":[{"type":"text","text":"ok"}]}],"meta":{"messageId":"m-ns"}}}"#;
        let no_source_facts = stated(&mut stream, no_source);
        assert!(
            no_source_facts
                .iter()
                .all(|f| !matches!(&f.kind, FactKind::Prompt(_)))
        );
        assert!(no_source_facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::ToolEnd { call, outcome: Outcome::Ok } if call == "call_01_edit"
        )));
        // Automated injections never prompt.
        let cron = r#"{"type":"message","id":"c","timestamp":"2026-09-30T10:00:06.000Z","message":{"role":"user","content":[{"type":"text","text":"scheduled tick"}],"meta":{"source":"scheduled-cron:job-1"}}}"#;
        assert!(
            stated(&mut stream, cron)
                .iter()
                .all(|f| !matches!(&f.kind, FactKind::Prompt(_)))
        );
        // An image-only user message is activity, not a prompt.
        let image = r#"{"type":"message","id":"i","timestamp":"2026-09-30T10:00:07.000Z","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}],"meta":{"source":"user","messageId":"m-i"}}}"#;
        assert_eq!(kinds(&stated(&mut stream, image)), ["Activity"]);

        // Folding: exactly the three human prompts form the spine.
        let mut model = SessionModel::new("demo".into());
        let mut stream = stream_at_header();
        for line in [
            text("user"),
            text("steering"),
            text("followup"),
            text("tool"),
            no_source.to_string(),
            cron.to_string(),
            image.to_string(),
        ] {
            if let Some(statement) = stream.push(&line) {
                for fact in &statement.facts {
                    model.apply_fact(fact);
                }
            }
        }
        assert_eq!(model.prompts.len(), 3);
        assert_eq!(model.prompts[0].excerpt, "hello from user");
    }

    #[test]
    fn tool_end_absent_is_error_is_success() {
        let use_line = r#"{"type":"message","id":"t1","timestamp":"2026-09-30T10:00:09.000Z","model":"m","message":{"role":"assistant","content":[{"type":"tool_use","id":"call_ok","name":"edit_file","input":{}}],"meta":{"source":"model","messageId":"m1"}}}"#;
        let mut stream = stream_at_header();
        let start = stated(&mut stream, use_line);
        assert!(start.iter().any(|f| matches!(
            &f.kind,
            FactKind::ToolStart { call, .. } if call == "call_ok"
        )));

        let result = |call: &str, extra: &str| {
            format!(
                r#"{{"type":"message","id":"t2","timestamp":"2026-09-30T10:00:10.000Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{call}"{extra}}}],"meta":{{"source":"tool","messageId":"m2"}}}}}}"#
            )
        };
        let ok = stated(&mut stream, &result("call_ok", ""));
        assert!(matches!(
            &ok[0].kind,
            FactKind::ToolEnd { call, outcome: Outcome::Ok } if call == "call_ok"
        ));
        assert_eq!(ok[0].agent.as_deref(), Some("main"));
        let err = stated(&mut stream, &result("call_ok", r#","is_error":true"#));
        assert!(matches!(
            &err[0].kind,
            FactKind::ToolEnd {
                outcome: Outcome::Err,
                ..
            }
        ));

        // One entry, two id-carrying results: one `ToolEnd` each; a result
        // without a `tool_use_id` states nothing.
        let batched = r#"{"type":"message","id":"t3","timestamp":"2026-09-30T10:00:11.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_a"},{"type":"tool_result","tool_use_id":"call_b"},{"type":"tool_result","content":[]}],"meta":{"source":"tool","messageId":"m3"}}}"#;
        let batched = stated(&mut stream, batched);
        assert_eq!(
            batched
                .iter()
                .filter(|f| matches!(&f.kind, FactKind::ToolEnd { .. }))
                .count(),
            2
        );
        // A tool_use without an id states nothing: the entry is only activity.
        let unnamed = r#"{"type":"message","id":"t4","timestamp":"2026-09-30T10:00:12.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"read_file"}],"meta":{"source":"model","messageId":"m4"}}}"#;
        assert_eq!(kinds(&stated(&mut stream, unnamed)), ["Activity"]);

        // Folding start + result leaves the call Ok and nothing pending.
        let mut model = SessionModel::new("demo".into());
        let mut stream = stream_at_header();
        for line in [use_line, &result("call_ok", "")] {
            if let Some(statement) = stream.push(line) {
                for fact in &statement.facts {
                    model.apply_fact(fact);
                }
            }
        }
        let calls: Vec<_> = model.agent("main").unwrap().tool_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].state, ToolState::Ok);
    }

    #[test]
    fn tool_start_summarizes_against_the_header_cwd() {
        let line = r#"{"type":"message","id":"t","timestamp":"2026-09-30T10:00:09.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"edit_file","input":{"file_path":"/project/src/exporter.rs"}},{"type":"tool_use","id":"c2","name":"shell_command","input":{"command":"cargo test --lib"}},{"type":"tool_use","id":"c3","name":"read_directory","input":{"path":"/project/src"}}],"meta":{"source":"model","messageId":"m"}}}"#;
        let mut stream = stream_at_header();
        let facts = stated(&mut stream, line);
        let start = |call: &str| {
            facts.iter().find_map(|f| match &f.kind {
                FactKind::ToolStart {
                    call: c,
                    name,
                    summary,
                } if c == call => Some((name.clone(), summary.clone())),
                _ => None,
            })
        };
        assert_eq!(
            start("c1"),
            Some(("edit_file".to_string(), Some("src/exporter.rs".to_string())))
        );
        assert_eq!(
            start("c2"),
            Some((
                "shell_command".to_string(),
                Some("cargo test --lib".to_string())
            ))
        );
        assert_eq!(
            start("c3"),
            Some(("read_directory".to_string(), Some("src".to_string())))
        );
    }

    #[test]
    fn tokens_dedup_key_is_message_id() {
        let entry = |output: u64, message_id: &str| {
            let meta = if message_id.is_empty() {
                String::new()
            } else {
                format!(r#","messageId":"{message_id}""#)
            };
            format!(
                r#"{{"type":"message","id":"x","timestamp":"2026-09-30T10:00:09.000Z","model":"claude-sonnet-4-5","usage":{{"outputTokens":{output}}},"message":{{"role":"assistant","content":[],"meta":{{"source":"model"{meta}}}}}}}"#
            )
        };
        let mut stream = Stream::new();
        let mut model = SessionModel::new("demo".into());
        let mut seen = Vec::new();
        let mut first_tokens = None;
        for (output, message_id) in [(64u64, "msg-2"), (120, "msg-3"), (10, "")] {
            for fact in stated(&mut stream, &entry(output, message_id)) {
                if let FactKind::Tokens { output, dedup } = &fact.kind {
                    seen.push((*output, dedup.clone()));
                    first_tokens.get_or_insert_with(|| fact.clone());
                }
                model.apply_fact(&fact);
            }
        }
        assert_eq!(
            seen,
            [
                (64, Some("msg-2".to_string())),
                (120, Some("msg-3".to_string())),
                (10, None),
            ]
        );
        assert_eq!(model.agent("main").unwrap().output_tokens, 194);
        // Re-applying an entry (a replay or rescan) does not double count.
        model.apply_fact(&first_tokens.expect("tokens stated"));
        assert_eq!(model.agent("main").unwrap().output_tokens, 194);
    }

    #[test]
    fn model_is_set_once_and_change_is_still_stated() {
        let assistant = r#"{"type":"message","id":"a","timestamp":"2026-09-30T10:00:09.000Z","model":"claude-sonnet-4-5","message":{"role":"assistant","content":[],"meta":{"source":"model","messageId":"m1"}}}"#;
        let change = r#"{"type":"model_change","id":"b","timestamp":"2026-09-30T10:02:00.000Z","model":"claude-opus-4-8"}"#;
        let mut stream = stream_at_header();
        let statement = stream.push(change).expect("a model_change states");
        assert_eq!(statement.facts.len(), 1);
        assert!(matches!(
            &statement.facts[0].kind,
            FactKind::Model(m) if m == "claude-opus-4-8"
        ));
        assert_eq!(statement.facts[0].agent.as_deref(), Some("main"));

        let mut model = SessionModel::new("demo".into());
        let mut fold = Stream::new();
        for line in [assistant, change] {
            if let Some(statement) = fold.push(line) {
                for fact in &statement.facts {
                    model.apply_fact(fact);
                }
            }
        }
        assert_eq!(
            model.agent("main").unwrap().model.as_deref(),
            Some("claude-sonnet-4-5"),
            "set-once: the first observed model wins"
        );
    }

    #[test]
    fn compaction_is_a_tally() {
        let line = r#"{"type":"compaction","id":"a11","timestamp":"2026-09-30T10:03:00.000Z","summary":"Trimmed.","firstKeptEntryId":"a6","tokensBefore":18000}"#;
        let mut stream = stream_at_header();
        let facts = stated(&mut stream, line);
        assert_eq!(kinds(&facts), ["Tally"]);
        assert_eq!(facts[0].agent, None);
        assert!(facts[0].is_session_meta());
        assert!(matches!(&facts[0].kind, FactKind::Tally(t) if t == "compaction"));
        let mut info = SessionInfo::default();
        for fact in &facts {
            info.apply(fact);
        }
        assert_eq!(info.tally("compaction"), 1);
    }

    #[test]
    fn effort_rows_are_deduped_by_change() {
        let message = |id: &str, effort: &str| {
            format!(
                r#"{{"type":"message","id":"{id}","timestamp":"2026-09-30T10:00:09.000Z","effort":"{effort}","message":{{"role":"assistant","content":[],"meta":{{"source":"model","messageId":"{id}"}}}}}}"#
            )
        };
        let mut stream = stream_at_header();
        let first = stated(&mut stream, &message("a", "high"));
        assert!(first.iter().any(|f| matches!(
            &f.kind,
            FactKind::Session { label, value } if label == "effort" && value == "high"
        )));
        // An effort string is session state, never agent naming.
        assert!(
            first
                .iter()
                .all(|f| !matches!(&f.kind, FactKind::Label { .. }))
        );
        // An unchanged repeat is not restated.
        let second = stated(&mut stream, &message("b", "high"));
        assert!(
            second
                .iter()
                .all(|f| !matches!(&f.kind, FactKind::Session { .. }))
        );
        // A change is.
        let change = r#"{"type":"effort_change","id":"c","timestamp":"2026-09-30T10:02:01.000Z","effort":"medium"}"#;
        let change_facts = stated(&mut stream, change);
        assert!(matches!(
            &change_facts[0].kind,
            FactKind::Session { label, value } if label == "effort" && value == "medium"
        ));
        assert_eq!(change_facts[0].agent, None);

        let mut info = SessionInfo::default();
        for fact in first.iter().chain(&second).chain(&change_facts) {
            info.apply(fact);
        }
        assert_eq!(info.field("effort"), Some("medium"));
        assert_eq!(
            info.fields.iter().filter(|(l, _)| l == "effort").count(),
            1,
            "one row, latest wins"
        );
    }

    #[test]
    fn spawn_trio_and_agent_output_is_not_a_spawn() {
        let spawn = r#"{"type":"message","id":"a4","timestamp":"2026-09-30T10:00:20.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Run the suite once before touching the flag path."},{"type":"tool_use","id":"call_02_agent","name":"agent","input":{"description":"Verify exporter tests pass","prompt":"Run the exporter tests and report the result."}}],"meta":{"source":"model","messageId":"msg-4"}}}"#;
        let mut stream = stream_at_header();
        let facts = stated(&mut stream, spawn);
        assert!(facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::ToolStart { call, name, summary }
                if call == "call_02_agent"
                    && name == "agent"
                    && summary.as_deref() == Some("Verify exporter tests pass")
        )));
        assert!(facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::Spawn { call } if call == "call_02_agent"
        )));
        let child = facts
            .iter()
            .find(|f| f.agent.as_deref() == Some("call_02_agent"))
            .expect("the child is born");
        assert!(matches!(
            &child.kind,
            FactKind::Agent {
                kind: AgentKind::Subagent,
                parent: Some(p),
                agent_type: None,
                description: Some(d),
                spawned_by: Some(c),
                interactive: false,
            } if p == "main"
                && d == "Verify exporter tests pass"
                && c == "call_02_agent"
        ));

        // `agent_output` polls; it does not spawn.
        let poll = r#"{"type":"message","id":"a5","timestamp":"2026-09-30T10:00:30.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"call_03_poll","name":"agent_output","input":{"agent_id":"bg-1-demo"}}],"meta":{"source":"model","messageId":"msg-5"}}}"#;
        let poll_facts = stated(&mut stream, poll);
        assert!(poll_facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::ToolStart { name, .. } if name == "agent_output"
        )));
        assert!(
            poll_facts
                .iter()
                .all(|f| !matches!(&f.kind, FactKind::Spawn { .. }))
        );

        // A spawn call's result states only its `ToolEnd` — the provider
        // never emits `Ended` for a child; the core joins the completion.
        let result = r#"{"type":"message","id":"a6","timestamp":"2026-09-30T10:01:00.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_02_agent","content":[{"type":"text","text":"agent_id: bg-1-demo"}]}],"meta":{"source":"tool","messageId":"msg-6"}}}"#;
        let result_facts = stated(&mut stream, result);
        assert_eq!(kinds(&result_facts), ["ToolEnd"]);
        assert!(matches!(
            &result_facts[0].kind,
            FactKind::ToolEnd { call, outcome: Outcome::Ok } if call == "call_02_agent"
        ));
        // An unseen call's result is still just its end.
        let unseen = r#"{"type":"message","id":"a7","timestamp":"2026-09-30T10:01:01.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_99_unseen"}],"meta":{"source":"tool","messageId":"msg-7"}}}"#;
        assert_eq!(kinds(&stated(&mut stream, unseen)), ["ToolEnd"]);
    }

    #[test]
    fn agent_completion_joins_in_the_core() {
        let prompt = r#"{"type":"message","id":"a1","timestamp":"2026-09-30T10:00:04.000Z","message":{"role":"user","content":[{"type":"text","text":"Add a --dry-run flag."}],"meta":{"source":"user","messageId":"msg-1"}}}"#;
        let spawn = r#"{"type":"message","id":"a4","timestamp":"2026-09-30T10:00:20.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Run the suite once before touching the flag path."},{"type":"tool_use","id":"call_02_agent","name":"agent","input":{"description":"Verify exporter tests pass","prompt":"Run the exporter tests and report the result."}}],"meta":{"source":"model","messageId":"msg-4"}}}"#;
        let result = r#"{"type":"message","id":"a6","timestamp":"2026-09-30T10:01:00.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_02_agent","content":[{"type":"text","text":"All exporter tests pass."}]}],"meta":{"source":"tool","messageId":"msg-6"}}}"#;

        let mut model = SessionModel::new("demo".into());
        let mut info = SessionInfo::default();
        let mut stream = Stream::new();
        for line in [HEADER, prompt, spawn, result] {
            if let Some(mut statement) = stream.push(line) {
                for fact in statement.take_session_meta() {
                    info.apply(&fact);
                }
                for fact in &statement.facts {
                    model.apply_fact(fact);
                }
            }
        }
        model.recompute_group_status();

        let child = model.agent("call_02_agent").expect("the child exists");
        assert_eq!(child.status, AgentStatus::Done);
        assert!(child.terminal, "the core's acknowledgement join pins it");
        let context = model.provenance(child).expect("provenance is recorded");
        assert_eq!(
            context.reasoning.as_deref(),
            Some("Run the suite once before touching the flag path.")
        );
        assert_eq!(
            model.provenance_prompt(context),
            Some("Add a --dry-run flag.")
        );
    }

    #[test]
    fn summarize_tool_covers_every_lexicon_row() {
        let json = |v: serde_json::Value| v;
        // shell_command: command, else description.
        assert_eq!(
            summarize_tool(
                "shell_command",
                &json(serde_json::json!({"command":"cargo test"})),
                None
            )
            .as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            summarize_tool(
                "shell_command",
                &json(serde_json::json!({"description":"run the tests"})),
                None
            )
            .as_deref(),
            Some("run the tests")
        );
        // read/write/edit: file_path, else path, else paths[0], short-pathed.
        assert_eq!(
            summarize_tool(
                "read_file",
                &json(serde_json::json!({"file_path":"/project/a.rs"})),
                Some("/project")
            )
            .as_deref(),
            Some("a.rs")
        );
        assert_eq!(
            summarize_tool(
                "write_file",
                &json(serde_json::json!({"path":"/project/b.rs"})),
                Some("/project")
            )
            .as_deref(),
            Some("b.rs")
        );
        assert_eq!(
            summarize_tool(
                "edit_file",
                &json(serde_json::json!({"paths":["/project/c.rs"]})),
                Some("/project")
            )
            .as_deref(),
            Some("c.rs")
        );
        // read_directory: path.
        assert_eq!(
            summarize_tool(
                "read_directory",
                &json(serde_json::json!({"path":"/project/src"})),
                Some("/project")
            )
            .as_deref(),
            Some("src")
        );
        // grep/glob: pattern.
        assert_eq!(
            summarize_tool(
                "grep",
                &json(serde_json::json!({"pattern":"fn main"})),
                None
            )
            .as_deref(),
            Some("fn main")
        );
        assert_eq!(
            summarize_tool(
                "glob",
                &json(serde_json::json!({"pattern":"**/*.rs"})),
                None
            )
            .as_deref(),
            Some("**/*.rs")
        );
        // web_search: query; web_fetch: url.
        assert_eq!(
            summarize_tool(
                "web_search",
                &json(serde_json::json!({"query":"rust clippy"})),
                None
            )
            .as_deref(),
            Some("rust clippy")
        );
        assert_eq!(
            summarize_tool(
                "web_fetch",
                &json(serde_json::json!({"url":"https://example.com/x"})),
                None
            )
            .as_deref(),
            Some("https://example.com/x")
        );
        // agent (the spawn set): description.
        assert_eq!(
            summarize_tool(
                "agent",
                &json(serde_json::json!({"description":"Verify exporter tests pass"})),
                None
            )
            .as_deref(),
            Some("Verify exporter tests pass")
        );
        // Anything else: description, else query, else command; nothing → None.
        assert_eq!(
            summarize_tool(
                "todo_write",
                &json(serde_json::json!({"description":"plan the work"})),
                None
            )
            .as_deref(),
            Some("plan the work")
        );
        assert_eq!(
            summarize_tool("mystery", &json(serde_json::json!({"query":"q"})), None).as_deref(),
            Some("q")
        );
        assert_eq!(
            summarize_tool("mystery", &json(serde_json::json!({"command":"c"})), None).as_deref(),
            Some("c")
        );
        assert_eq!(
            summarize_tool("mystery", &json(serde_json::json!({})), None),
            None
        );
    }

    #[test]
    fn activity_fallback_and_unknown_entries_skip() {
        let mut stream = stream_at_header();
        let image = r#"{"type":"message","id":"i","timestamp":"2026-09-30T10:00:07.000Z","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}],"meta":{"source":"user","messageId":"m-i"}}}"#;
        let facts = stated(&mut stream, image);
        assert_eq!(kinds(&facts), ["Activity"]);
        assert_eq!(facts[0].agent.as_deref(), Some("main"));
        // No role at all: activity.
        let empty = r#"{"type":"message","id":"e","timestamp":"2026-09-30T10:00:08.000Z","message":{"content":[]}}"#;
        assert_eq!(kinds(&stated(&mut stream, empty)), ["Activity"]);
        // Unknown types, blank and malformed lines state nothing.
        assert!(
            stream
                .push(r#"{"type":"label","value":"ignored"}"#)
                .is_none()
        );
        assert!(stream.push("").is_none());
        assert!(stream.push("not json").is_none());
    }

    #[test]
    fn push_entry_is_the_same_as_pushing_the_line() {
        let entry = wire::parse_line(HEADER).expect("parses");
        let mut via_entry = Stream::new();
        let mut via_line = Stream::new();
        assert_eq!(via_entry.push_entry(&entry), via_line.push(HEADER));

        let change = wire::parse_line(r#"{"type":"model_change","id":"x","timestamp":"2026-09-30T10:02:00.000Z","model":"claude-opus-4-8"}"#)
            .expect("parses");
        let mut stream = stream_at_header();
        assert!(stream.push_entry(&change).is_some());
    }

    /// The shipped demo session through the whole conformance check: order
    /// invariance plus the model and timeline goldens beside the fixture.
    #[test]
    fn demo_session_conforms() {
        let Some(dir) = crate::provider::harness::fixture_dir("commandcode") else {
            return;
        };
        let demo = dir.join("demo.jsonl");
        let streams = || {
            let text = std::fs::read_to_string(&demo).unwrap();
            let mut stream = Stream::new();
            // Single-file sessions: exactly one stream, in file order.
            vec![text.lines().filter_map(|l| stream.push(l)).collect()]
        };
        crate::provider::harness::conform("commandcode", "demo", streams);
    }

    /// The fixture opens by path the way `zoe <file>` does: content decides
    /// the provider, the header id is the session, and every file of the
    /// session is the one root.
    #[test]
    fn demo_opens_by_path_as_commandcode() {
        let Some(dir) = crate::provider::harness::fixture_dir("commandcode") else {
            return;
        };
        let demo = dir.join("demo.jsonl");
        let session = crate::provider::open(&crate::provider::Target::Path(demo.clone()), None)
            .expect("the fixture opens");
        assert_eq!(session.provider, crate::provider::Provider::CommandCode);
        assert_eq!(session.id, "demo");
        assert_eq!(session.root.path, demo);
        assert_eq!(session.files.len(), 0, "sessions are single-file");
        let mut every = session.every_file();
        assert_eq!(every.next().map(|f| f.path.clone()), Some(demo));
        assert!(every.next().is_none());
    }

    // S-008
    #[test]
    fn s_008_header_states_root_and_session_rows() {
        let mut stream = Stream::new();
        let statement = stream.push(HEADER).expect("the header states");
        assert_eq!(statement.at, Some(at("2026-09-30T10:00:00Z")));
        assert_eq!(statement.facts.len(), 3);
        let agent = &statement.facts[0];
        assert_eq!(agent.agent.as_deref(), Some("main"));
        assert_eq!(agent.ts, Some(at("2026-09-30T10:00:00Z")));
        assert!(matches!(
            &agent.kind,
            FactKind::Agent {
                kind: AgentKind::Main,
                parent: None,
                agent_type: Some(t),
                description: None,
                spawned_by: None,
                interactive: true,
            } if t == "commandcode"
        ));
        assert!(matches!(
            &statement.facts[1].kind,
            FactKind::Session { label, value } if label == "cwd" && value == "/project"
        ));
        assert_eq!(statement.facts[1].agent, None);
        assert!(statement.facts[1].is_session_meta());
        assert!(matches!(
            &statement.facts[2].kind,
            FactKind::Session { label, value } if label == "version" && value == "3"
        ));
        assert!(statement.facts[2].is_session_meta());

        // Row 1 is first-seen-only: a repeated header restates nothing.
        assert!(stream.push(HEADER).is_none());

        let mut model = SessionModel::new("demo".into());
        for fact in &statement.facts {
            model.apply_fact(fact);
        }
        assert_eq!(
            model.agent("main").unwrap().agent_type.as_deref(),
            Some("commandcode"),
            "the name SessionModel::provider reads"
        );
    }

    // S-009
    #[test]
    fn s_009_prompt_follows_source_allowlist() {
        let human = r#"{"type":"message","id":"p1","timestamp":"2026-09-30T10:00:04.000Z","message":{"role":"user","content":[{"type":"text","text":"Add a --dry-run flag to the exporter and run the tests."}],"meta":{"source":"user","messageId":"msg-1"}}}"#;
        let tool = r#"{"type":"message","id":"p2","timestamp":"2026-09-30T10:00:10.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_01_edit","content":[{"type":"text","text":"Edited src/exporter.rs"}]}],"meta":{"source":"tool","messageId":"msg-2"}}}"#;
        let steering = r#"{"type":"message","id":"p3","timestamp":"2026-09-30T10:01:05.000Z","message":{"role":"user","content":[{"type":"text","text":"Stop after the tests; do not commit."}],"meta":{"source":"steering","messageId":"msg-3"}}}"#;
        let followup = r#"{"type":"message","id":"p4","timestamp":"2026-09-30T10:01:06.000Z","message":{"role":"user","content":[{"type":"text","text":"Also update the docs."}],"meta":{"source":"followup","messageId":"msg-4"}}}"#;
        let no_source = r#"{"type":"message","id":"p5","timestamp":"2026-09-30T10:01:07.000Z","message":{"role":"user","content":[{"type":"text","text":"Error: 503 Service Unavailable"}],"meta":{"messageId":"msg-5"}}}"#;
        let cron = r#"{"type":"message","id":"p6","timestamp":"2026-09-30T10:01:08.000Z","message":{"role":"user","content":[{"type":"text","text":"scheduled tick"}],"meta":{"source":"scheduled-cron:job-1"}}}"#;
        let automated = r#"{"type":"message","id":"p7","timestamp":"2026-09-30T10:01:09.000Z","message":{"role":"user","content":[{"type":"text","text":"Automated but human-sourced."}],"meta":{"source":"user","messageId":"msg-7","isAutomated":true}}}"#;
        let image = r#"{"type":"message","id":"p8","timestamp":"2026-09-30T10:01:10.000Z","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}],"meta":{"source":"user","messageId":"msg-8"}}}"#;

        let mut stream = stream_at_header();
        let prompt_of = |facts: &[Fact]| -> Option<String> {
            facts.iter().find_map(|f| match &f.kind {
                FactKind::Prompt(text) => Some(text.clone()),
                _ => None,
            })
        };

        let facts = stated(&mut stream, human);
        assert_eq!(
            prompt_of(&facts).as_deref(),
            Some("Add a --dry-run flag to the exporter and run the tests.")
        );

        let facts = stated(&mut stream, tool);
        assert_eq!(prompt_of(&facts), None);
        assert!(facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::ToolEnd { call, outcome: Outcome::Ok } if call == "call_01_edit"
        )));

        for (line, text) in [
            (steering, "Stop after the tests; do not commit."),
            (followup, "Also update the docs."),
            (automated, "Automated but human-sourced."),
        ] {
            assert_eq!(prompt_of(&stated(&mut stream, line)).as_deref(), Some(text));
        }

        // Absent source, automated injections and image-only text never prompt.
        assert_eq!(kinds(&stated(&mut stream, no_source)), ["Activity"]);
        assert_eq!(kinds(&stated(&mut stream, cron)), ["Activity"]);
        assert_eq!(kinds(&stated(&mut stream, image)), ["Activity"]);

        // Folding leaves exactly the four human prompts as the spine.
        let mut model = SessionModel::new("demo".into());
        let mut stream = stream_at_header();
        for line in [
            human, tool, steering, followup, no_source, cron, automated, image,
        ] {
            if let Some(statement) = stream.push(line) {
                for fact in &statement.facts {
                    model.apply_fact(fact);
                }
            }
        }
        let excerpts: Vec<&str> = model.prompts.iter().map(|p| p.excerpt.as_str()).collect();
        assert_eq!(
            excerpts,
            [
                "Add a --dry-run flag to the exporter and run the tests.",
                "Stop after the tests; do not commit.",
                "Also update the docs.",
                "Automated but human-sourced.",
            ]
        );
    }

    // S-010
    #[test]
    fn s_010_tool_start_end_and_success_default() {
        let use_line = r#"{"type":"message","id":"t1","timestamp":"2026-09-30T10:00:09.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"call_01_edit","name":"edit_file","input":{"file_path":"/project/src/exporter.rs"}}],"meta":{"source":"model","messageId":"m1"}}}"#;
        let ok_line = r#"{"type":"message","id":"t2","timestamp":"2026-09-30T10:00:10.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_01_edit","content":[{"type":"text","text":"ok"}]}],"meta":{"source":"tool","messageId":"m2"}}}"#;
        let err_line = r#"{"type":"message","id":"t3","timestamp":"2026-09-30T10:00:11.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_02_fail","is_error":true}],"meta":{"source":"tool","messageId":"m3"}}}"#;
        let batched = r#"{"type":"message","id":"t4","timestamp":"2026-09-30T10:00:12.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_03_a"},{"type":"tool_result","tool_use_id":"call_03_b"},{"type":"tool_result","content":[]}],"meta":{"source":"tool","messageId":"m4"}}}"#;
        let no_id_result = r#"{"type":"message","id":"t5","timestamp":"2026-09-30T10:00:13.000Z","message":{"role":"user","content":[{"type":"tool_result","content":[]}],"meta":{"source":"tool","messageId":"m5"}}}"#;
        let no_id_use = r#"{"type":"message","id":"t6","timestamp":"2026-09-30T10:00:14.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"read_file","input":{}}],"meta":{"source":"model","messageId":"m6"}}}"#;

        let mut stream = stream_at_header();
        let facts = stated(&mut stream, use_line);
        assert!(matches!(
            &facts[0].kind,
            FactKind::ToolStart { call, name, summary }
                if call == "call_01_edit"
                    && name == "edit_file"
                    && summary.as_deref() == Some("src/exporter.rs")
        ));
        let facts = stated(&mut stream, ok_line);
        assert!(matches!(
            &facts[0].kind,
            FactKind::ToolEnd { call, outcome: Outcome::Ok } if call == "call_01_edit"
        ));
        let facts = stated(&mut stream, err_line);
        assert!(matches!(
            &facts[0].kind,
            FactKind::ToolEnd { call, outcome: Outcome::Err } if call == "call_02_fail"
        ));
        let facts = stated(&mut stream, batched);
        let ends: Vec<&str> = facts
            .iter()
            .filter_map(|f| match &f.kind {
                FactKind::ToolEnd { call, .. } => Some(call.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ends, ["call_03_a", "call_03_b"], "one ToolEnd per id");

        // A result without a `tool_use_id` and a tool_use without an `id`
        // state nothing; the entry is only activity (row 20).
        assert_eq!(kinds(&stated(&mut stream, no_id_result)), ["Activity"]);
        assert_eq!(kinds(&stated(&mut stream, no_id_use)), ["Activity"]);

        // Folding start + result leaves the call Ok and nothing pending.
        let mut model = SessionModel::new("demo".into());
        let mut stream = stream_at_header();
        for line in [use_line, ok_line] {
            if let Some(statement) = stream.push(line) {
                for fact in &statement.facts {
                    model.apply_fact(fact);
                }
            }
        }
        let calls: Vec<_> = model.agent("main").unwrap().tool_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].state, ToolState::Ok);
        assert!(calls.iter().all(|t| t.state != ToolState::Pending));
    }

    // S-011
    #[test]
    fn s_011_tokens_dedup_key_and_model_set_once() {
        let entry = |output: u64, message_id: Option<&str>| -> String {
            let meta = message_id
                .map(|id| format!(r#","messageId":"{id}""#))
                .unwrap_or_default();
            format!(
                r#"{{"type":"message","id":"m-{output}","timestamp":"2026-09-30T10:00:09.000Z","model":"claude-sonnet-4-5","usage":{{"outputTokens":{output}}},"message":{{"role":"assistant","content":[],"meta":{{"source":"model"{meta}}}}}}}"#
            )
        };

        let mut stream = stream_at_header();
        let first = stated(&mut stream, &entry(64, Some("msg-2")));
        assert!(first.iter().any(|f| matches!(
            &f.kind,
            FactKind::Model(m) if m == "claude-sonnet-4-5"
        ) && f.agent.as_deref() == Some("main")));
        let first_tokens = first
            .iter()
            .find(|f| matches!(&f.kind, FactKind::Tokens { .. }))
            .expect("the first entry states tokens");
        assert!(matches!(
            &first_tokens.kind,
            FactKind::Tokens { output: 64, dedup: Some(k) } if k == "msg-2"
        ));
        assert_eq!(first_tokens.agent.as_deref(), Some("main"));

        let mut model = SessionModel::new("demo".into());
        for fact in &first {
            model.apply_fact(fact);
        }
        let mut third_was_keyless = false;
        for (output, message_id) in [(120u64, Some("msg-3")), (10, None)] {
            let facts = stated(&mut stream, &entry(output, message_id));
            if output == 10 {
                third_was_keyless = facts.iter().any(|f| {
                    matches!(
                        &f.kind,
                        FactKind::Tokens {
                            output: 10,
                            dedup: None
                        }
                    )
                });
            }
            for fact in &facts {
                model.apply_fact(fact);
            }
        }
        assert!(third_was_keyless, "a missing messageId sums, dedup None");
        assert_eq!(model.agent("main").unwrap().output_tokens, 194);

        // A re-applied entry (replay or rescan) does not double count.
        model.apply_fact(first_tokens);
        assert_eq!(model.agent("main").unwrap().output_tokens, 194);

        // Model is set-once: a later Model fact is a fold no-op.
        model.apply_fact(&main_fact(FactKind::Model("claude-opus-4-8".into()), None));
        assert_eq!(
            model.agent("main").unwrap().model.as_deref(),
            Some("claude-sonnet-4-5")
        );
    }

    // S-012
    #[test]
    fn s_012_meta_entries_map_to_frozen_facts() {
        let effort_message = |id: &str| {
            format!(
                r#"{{"type":"message","id":"{id}","timestamp":"2026-09-30T10:00:09.000Z","effort":"high","message":{{"role":"assistant","content":[],"meta":{{"source":"model","messageId":"{id}"}}}}}}"#
            )
        };
        let compaction = r#"{"type":"compaction","id":"a11","parentId":"a10","timestamp":"2026-09-30T10:03:00.000Z","summary":"Trimmed the fixture conversation to its load-bearing turns.","firstKeptEntryId":"a6","tokensBefore":18000}"#;
        let session_info = r#"{"type":"session_info","id":"a12","parentId":"a11","timestamp":"2026-09-30T10:03:01.000Z","name":"Exporter dry-run flag"}"#;
        let model_change = r#"{"type":"model_change","id":"a9","parentId":"a8","timestamp":"2026-09-30T10:02:00.000Z","model":"claude-opus-4-8"}"#;
        let effort_change = r#"{"type":"effort_change","id":"a10","parentId":"a9","timestamp":"2026-09-30T10:02:01.000Z","effort":"medium"}"#;

        let mut stream = stream_at_header();
        let mut all: Vec<Fact> = Vec::new();
        let mut push = |stream: &mut Stream, line: &str| -> Vec<Fact> {
            let facts = stated(stream, line);
            all.extend(facts.iter().cloned());
            facts
        };

        // The first effort observation is stated; an adjacent identical
        // repeat is not restated.
        let first = push(&mut stream, &effort_message("a2"));
        assert!(first.iter().any(|f| matches!(
            &f.kind,
            FactKind::Session { label, value } if label == "effort" && value == "high"
        ) && f.agent.is_none()));
        let second = push(&mut stream, &effort_message("a4"));
        assert!(
            second
                .iter()
                .all(|f| !matches!(&f.kind, FactKind::Session { .. })),
            "an unchanged repeat states no effort fact"
        );

        let facts = push(&mut stream, compaction);
        assert_eq!(kinds(&facts), ["Tally"]);
        assert_eq!(facts[0].agent, None);
        assert!(facts[0].is_session_meta());
        assert!(matches!(&facts[0].kind, FactKind::Tally(t) if t == "compaction"));

        let facts = push(&mut stream, session_info);
        assert_eq!(kinds(&facts), ["Title"]);
        assert!(matches!(
            &facts[0].kind,
            FactKind::Title(t) if t == "Exporter dry-run flag"
        ));

        let facts = push(&mut stream, model_change);
        assert_eq!(kinds(&facts), ["Model"]);
        assert_eq!(facts[0].agent.as_deref(), Some("main"));
        assert!(matches!(
            &facts[0].kind,
            FactKind::Model(m) if m == "claude-opus-4-8"
        ));

        let facts = push(&mut stream, effort_change);
        assert!(matches!(
            &facts[0].kind,
            FactKind::Session { label, value } if label == "effort" && value == "medium"
        ));
        assert_eq!(facts[0].agent, None);

        // An effort string is session state, never agent naming.
        assert!(
            all.iter()
                .all(|f| !matches!(&f.kind, FactKind::Label { .. })),
            "no Label fact anywhere"
        );

        // Folding in file order: one row, latest wins; the title lands.
        let mut info = SessionInfo::default();
        for fact in &all {
            info.apply(fact);
        }
        assert_eq!(info.field("effort"), Some("medium"));
        assert_eq!(info.title.as_deref(), Some("Exporter dry-run flag"));
        assert_eq!(info.fields.iter().filter(|(l, _)| l == "effort").count(), 1);
    }

    // S-013
    #[test]
    fn s_013_spawn_trio_and_completion_join() {
        let prompt = r#"{"type":"message","id":"a1","timestamp":"2026-09-30T10:00:04.000Z","message":{"role":"user","content":[{"type":"text","text":"Add a --dry-run flag to the exporter and run the tests."}],"meta":{"source":"user","messageId":"msg-1"}}}"#;
        let spawn = r#"{"type":"message","id":"a4","timestamp":"2026-09-30T10:00:20.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Run the suite once before touching the flag path."},{"type":"tool_use","id":"call_02_agent","name":"agent","input":{"description":"Verify exporter tests pass","prompt":"Run the exporter tests and report the result."}}],"meta":{"source":"model","messageId":"msg-4"}}}"#;
        let result = r#"{"type":"message","id":"a5","timestamp":"2026-09-30T10:01:00.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_02_agent","content":[{"type":"text","text":"agent_id: bg-1-demo\nstatus: completed\n\nAll exporter tests pass."}]}],"meta":{"source":"tool","messageId":"msg-5"}}}"#;
        let poll = r#"{"type":"message","id":"a6","timestamp":"2026-09-30T10:01:10.000Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"call_03_poll","name":"agent_output","input":{"agent_id":"bg-1-demo"}}],"meta":{"source":"model","messageId":"msg-6"}}}"#;
        let unseen = r#"{"type":"message","id":"a7","timestamp":"2026-09-30T10:01:20.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_99_unseen","content":[{"type":"text","text":"late"}]}],"meta":{"source":"tool","messageId":"msg-7"}}}"#;

        let mut stream = stream_at_header();
        let facts = stated(&mut stream, prompt);
        assert!(matches!(
            &facts[0].kind,
            FactKind::Prompt(t) if t == "Add a --dry-run flag to the exporter and run the tests."
        ));

        let facts = stated(&mut stream, spawn);
        assert!(facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::ToolStart { call, name, summary }
                if call == "call_02_agent"
                    && name == "agent"
                    && summary.as_deref() == Some("Verify exporter tests pass")
        )));
        assert!(facts.iter().any(|f| matches!(
            &f.kind,
            FactKind::Spawn { call } if call == "call_02_agent"
        )));
        let child = facts
            .iter()
            .find(|f| f.agent.as_deref() == Some("call_02_agent"))
            .expect("the child is born");
        assert!(matches!(
            &child.kind,
            FactKind::Agent {
                kind: AgentKind::Subagent,
                parent: Some(p),
                agent_type: None,
                description: Some(d),
                spawned_by: Some(c),
                interactive: false,
            } if p == "main"
                && d == "Verify exporter tests pass"
                && c == "call_02_agent"
        ));

        // The spawn call's own result states ONLY its ToolEnd — never an
        // `Ended`; the child's completion is the core's join.
        let facts = stated(&mut stream, result);
        assert_eq!(kinds(&facts), ["ToolEnd"]);
        assert!(matches!(
            &facts[0].kind,
            FactKind::ToolEnd { call, outcome: Outcome::Ok } if call == "call_02_agent"
        ));
        assert_eq!(facts[0].agent.as_deref(), Some("main"));
        assert!(facts.iter().all(|f| !matches!(&f.kind, FactKind::Ended(_))));

        // `agent_output` polls; it does not spawn.
        let facts = stated(&mut stream, poll);
        assert_eq!(kinds(&facts), ["ToolStart"]);
        assert!(
            facts
                .iter()
                .all(|f| !matches!(&f.kind, FactKind::Spawn { .. }))
        );

        // An unseen call's result is still just its end.
        let facts = stated(&mut stream, unseen);
        assert_eq!(kinds(&facts), ["ToolEnd"]);

        // The core joins the acknowledgement: the child is Done and carries
        // its provenance (the prompt and the reasoning above the call).
        let mut model = SessionModel::new("demo".into());
        let mut stream = stream_at_header();
        for line in [prompt, spawn, result] {
            if let Some(statement) = stream.push(line) {
                for fact in &statement.facts {
                    model.apply_fact(fact);
                }
            }
        }
        model.recompute_group_status();
        let child = model.agent("call_02_agent").expect("the child exists");
        assert_eq!(child.status, AgentStatus::Done);
        assert!(child.terminal, "the acknowledgement join pins it");
        let context = model.provenance(child).expect("provenance is recorded");
        assert_eq!(
            context.reasoning.as_deref(),
            Some("Run the suite once before touching the flag path.")
        );
        assert_eq!(
            model.provenance_prompt(context),
            Some("Add a --dry-run flag to the exporter and run the tests.")
        );
    }

    // S-016
    #[test]
    fn s_016_demo_fixture_opens_and_conforms() {
        let Some(dir) = crate::provider::harness::fixture_dir("commandcode") else {
            return;
        };
        let demo = dir.join("demo.jsonl");

        let session = crate::provider::open(&crate::provider::Target::Path(demo.clone()), None)
            .expect("the fixture opens by path");
        assert_eq!(session.provider, crate::provider::Provider::CommandCode);
        assert_eq!(session.id, "demo");
        assert_eq!(session.root.path, demo);
        assert_eq!(session.files.len(), 0, "single-file sessions");
        let mut every = session.every_file();
        assert_eq!(every.next().map(|f| f.path.clone()), Some(demo.clone()));
        assert!(every.next().is_none());

        // The goldens are the human-readable record of what the fixture
        // extracts.
        let model_golden = std::fs::read_to_string(dir.join("demo.model.txt")).expect("golden");
        assert!(model_golden.contains("[main] commandcode"));
        assert!(model_golden.contains("[subagent] Verify exporter tests pass"));
        let timeline = std::fs::read_to_string(dir.join("demo.timeline.txt")).expect("golden");
        assert!(timeline.contains("ToolEnd@main"));
        assert!(timeline.contains("Agent@call_02_agent"));
        assert!(
            !timeline.contains("Ended@"),
            "the provider never emits Ended"
        );
        assert!(
            !timeline.contains("Title@"),
            "session metadata is off-timeline"
        );
        let stamps: Vec<&str> = timeline
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.split_whitespace().next().unwrap())
            .collect();
        assert!(!stamps.is_empty(), "a dated timeline");
        for stamp in &stamps {
            chrono::DateTime::parse_from_rfc3339(stamp)
                .unwrap_or_else(|_| panic!("dated: {stamp}"));
        }
        assert!(
            stamps.windows(2).all(|w| w[0] <= w[1]),
            "sorted: {stamps:?}"
        );

        // The plain conformance run: order invariance plus both goldens.
        let streams = || {
            let text = std::fs::read_to_string(&demo).unwrap();
            let mut stream = Stream::new();
            vec![text.lines().filter_map(|l| stream.push(l)).collect()]
        };
        crate::provider::harness::conform("commandcode", "demo", streams);
    }
}
