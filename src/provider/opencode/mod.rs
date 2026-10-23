//! The opencode provider: SQLite rows in, [`Fact`]s out.
//!
//! One database file holds every session, so one [`Stream`] reads one
//! session: the feeder hands it the session's metadata, then each message
//! with its parts, in time order. The stream is idempotent by row id —
//! re-reading the database states only rows it has not seen, which is what
//! lets the live tailer poll the file without duplicates.
//!
//! What this module knows that the model must not: that a `user` text part
//! is a human prompt and an `assistant` text part is a label; that `task` is
//! the tool that spawns a subagent (whose transcript is a *separate* session,
//! joined by `parent_id` — no synthetic child node is stated here); that a
//! `step-finish` carries the turn's token count while the message-level
//! `tokens` would double-count it; that a turn's `finish` reason and a
//! `step-start` state nothing; and that a tool part without a `completed`
//! status is still running, so no `ToolEnd` is stated for it.
//!
//! What it deliberately does not say: no `Ended` is ever emitted. The format
//! has no per-agent completion record — a `step-finish` ends a *turn*, and
//! stating it as an ending would pin the agent `Done` while it keeps working.

use std::collections::HashSet;

use chrono::{DateTime, Utc};

pub mod discovery;
pub mod wire;

use crate::fact::{AgentKind, Fact, FactKind, Outcome, Statement};
use crate::provider::summary::{short_path, truncate_summary};
use crate::state::session::MAIN_ID;
use wire::{ExportRow, MessageData, PartData, SessionMeta, ms_time};

/// The seen-set key for a part row: its id plus a hash of its content, so an
/// in-place UPDATE of the same row re-states while an identical re-read stays
/// silent. `PartData` holds unhashable JSON (`ToolState::input`), so the hash
/// runs over the Debug rendering, which covers every field.
fn part_key(part_id: &str, part: &PartData) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{part:?}").hash(&mut h);
    format!("part/{part_id}#{:016x}", h.finish())
}

/// One opencode session being read: which rows it has stated already, and
/// what it has learned about the session along the way.
#[derive(Debug, Clone, Default)]
pub struct Stream {
    /// Whether the root agent has been stated.
    announced: bool,
    /// The session directory, for relativising paths in summaries.
    cwd: Option<String>,
    /// Row ids already stated (`session/…`, `message/…`, `part/…` prefixes).
    /// A re-read states only what is new. Part keys carry a content hash
    /// (see [`part_key`]): an in-place UPDATE re-states under a new key, so
    /// the set also retains one stale key per mutated row.
    seen: HashSet<String>,
    /// The database fingerprint at the last successful read, for the live
    /// tick gate in [`statements_for`]: an unchanged fingerprint skips the
    /// full message+part scan, keeping idle ticks to a stat.
    #[cfg(feature = "native")]
    db_stat: Option<(std::time::SystemTime, u64)>,
    /// The agent name already carried as a label on the root, if any.
    named: Option<String>,
}

impl Stream {
    pub fn new() -> Self {
        Stream::default()
    }

    /// Parse one exported line and state what it says (see
    /// [`wire::ExportRow`]). `None` for a blank or unparsable line, a repeat,
    /// or a row that states nothing.
    pub fn push_json(&mut self, line: &str) -> Option<Statement> {
        match wire::parse_export_row(line)? {
            ExportRow::Session(row) => self.announce(&SessionMeta::from(&row)),
            ExportRow::Message(row) => self.push_message(&row.id, &MessageData::from(&row.data)),
            ExportRow::Part(row) => {
                self.push_row(&MessageData::from(&row.message), &row.id, &row.data)
            }
        }
    }

    /// Parse one line and state what it says. The line-push contract the
    /// feeders share; for this provider a line is one exported row.
    pub fn push(&mut self, line: &str) -> Option<Statement> {
        self.push_json(line)
    }

    /// State one typed database row: a part with its message for context.
    /// The live feeder's way in (no JSON round-trip). Idempotent by part id
    /// AND content: a re-read states only what is new, while an in-place
    /// UPDATE of the same row (a tool part going `running` → `completed`)
    /// re-states under its new hash — otherwise the `ToolEnd` would never be
    /// stated. An identical re-push stays silent.
    pub fn push_row(
        &mut self,
        msg: &MessageData,
        part_id: &str,
        part: &PartData,
    ) -> Option<Statement> {
        if !self.seen.insert(part_key(part_id, part)) {
            return None;
        }
        // A step's counts are known when its message completes, so the
        // statement dates there; every other part dates at its own time,
        // else its message's.
        let at = match part {
            PartData::StepFinish { .. } => ms_time(msg.completed_ms).or_else(|| msg_time(msg)),
            _ => part_time(part).or_else(|| msg_time(msg)),
        };
        let mut facts = part_facts(msg, part, self.cwd.as_deref());
        if facts.is_empty() {
            return None;
        }
        for f in &mut facts {
            if f.ts.is_none() {
                f.ts = at;
            }
        }
        if let Some(birth) = self.ensure_announced(msg.agent.as_deref(), at) {
            facts.insert(0, birth);
        }
        Some(Statement { at, facts })
    }

    /// State a message's own facts: the model it ran and, once, the agent
    /// name it ran as. `None` when the message carries neither, or repeats.
    pub fn push_message(&mut self, id: &str, msg: &MessageData) -> Option<Statement> {
        if !self.seen.insert(format!("message/{id}")) {
            return None;
        }
        let at = msg_time(msg);
        let mut facts = Vec::new();
        if let Some(agent) = msg.agent.as_deref()
            && self.named.as_deref() != Some(agent)
        {
            self.named = Some(agent.to_string());
            facts.push(Fact {
                agent: Some(MAIN_ID.to_string()),
                ts: at,
                kind: FactKind::Label {
                    agent_type: Some(agent.to_string()),
                    description: None,
                },
            });
        }
        if let Some(model) = model_of(msg) {
            facts.push(Fact {
                agent: Some(MAIN_ID.to_string()),
                ts: at,
                kind: FactKind::Model(model),
            });
        }
        if facts.is_empty() {
            return None;
        }
        if let Some(birth) = self.ensure_announced(msg.agent.as_deref(), at) {
            facts.insert(0, birth);
        }
        Some(Statement { at, facts })
    }

    /// State the session itself: the root agent, its title and directory.
    /// `None` on a re-read.
    pub fn announce(&mut self, meta: &SessionMeta) -> Option<Statement> {
        if !self.seen.insert(format!("session/{}", meta.id)) {
            return None;
        }
        self.cwd = Some(meta.directory.clone());
        let at = ms_time(Some(meta.time_created));
        let mut facts = vec![Fact {
            agent: Some(MAIN_ID.to_string()),
            ts: at,
            kind: FactKind::Agent {
                kind: AgentKind::Main,
                parent: None,
                agent_type: meta.agent.clone().or_else(|| Some("opencode".into())),
                description: None,
                spawned_by: None,
                interactive: true,
            },
        }];
        if !meta.title.trim().is_empty() {
            facts.push(Fact {
                agent: None,
                ts: None,
                kind: FactKind::Title(meta.title.clone()),
            });
        }
        if !meta.directory.trim().is_empty() {
            facts.push(Fact {
                agent: None,
                ts: None,
                kind: FactKind::Session {
                    label: "cwd".into(),
                    value: meta.directory.clone(),
                },
            });
        }
        self.announced = true;
        self.named = meta.agent.clone();
        Some(Statement { at, facts })
    }

    /// A whole session: metadata, then each message's own facts and parts in
    /// time order. The database feeder's way in; idempotent by row id.
    pub fn feed_session(
        &mut self,
        meta: &SessionMeta,
        messages: &wire::SessionRows,
    ) -> Vec<Statement> {
        let mut out = Vec::new();
        out.extend(self.announce(meta));
        for (id, msg, parts) in messages {
            out.extend(self.push_message(id, msg));
            for (part_id, part) in parts {
                out.extend(self.push_row(msg, part_id, part));
            }
        }
        out
    }

    /// The root agent, stated once, on the first dated statement. Rows before
    /// any timestamp (there should be none) ride without it.
    fn ensure_announced(&mut self, agent: Option<&str>, at: Option<DateTime<Utc>>) -> Option<Fact> {
        if self.announced || at.is_none() {
            return None;
        }
        self.announced = true;
        if self.named.is_none() {
            self.named = agent.map(str::to_owned);
        }
        Some(Fact {
            agent: Some(MAIN_ID.to_string()),
            ts: at,
            kind: FactKind::Agent {
                kind: AgentKind::Main,
                parent: None,
                agent_type: self.named.clone().or_else(|| Some("opencode".into())),
                description: None,
                spawned_by: None,
                interactive: true,
            },
        })
    }
}

/// A whole session's statements from the live database. Empty when the
/// database cannot be read (locked, mid-compaction) — the feeder retries on
/// its next poll — and on a re-read only what is new, via the stream's
/// seen-ids.
///
/// Tick cost: the gate below stats the database files (main + WAL/sidecar
/// journals) and skips the full message+part scan while the fingerprint is
/// unchanged, so an idle 200ms tick costs one stat, not three connections and
/// a full table scan. The fingerprint updates only after a successful read,
/// so a failed read (lock) retries on the next tick, and the FIRST read of a
/// stream always runs (`db_stat` starts `None`).
#[cfg(feature = "native")]
pub fn statements_for(
    db: &std::path::Path,
    session_id: &str,
    stream: &mut Stream,
) -> Vec<Statement> {
    if let Some(fp) = discovery::db_fingerprint(db)
        && stream.db_stat == Some(fp)
    {
        return Vec::new();
    }
    let Some((meta, messages)) = discovery::read_session(db, session_id) else {
        return Vec::new();
    };
    stream.db_stat = discovery::db_fingerprint(db);
    stream.feed_session(&meta, &messages)
}

/// Which spawning tool this is. `task` runs a subagent; every other tool
/// only works. Deliberately an exact match: a census of the shipped fixture
/// (`assets/opencode/demo.jsonl`) finds `task` as the only spawning tool, and
/// a novel tool name must stay a plain `ToolStart` — never a `Spawn`, which
/// would invent a child session that does not exist. If a future census of a
/// live database (Counter over `part.data->>'tool'` for `type = 'tool'` rows)
/// finds sibling spawner names, widen this match then, with a fixture row.
pub fn is_spawn_tool(name: &str) -> bool {
    name == "task"
}

// ---------------------------------------------------------------------------
// Mapping: rows to facts
// ---------------------------------------------------------------------------

/// The model a message ran: the user message's nested `model.modelID`, or
/// the assistant message's flat `modelID`, falling back to either
/// `providerID`.
fn model_of(msg: &MessageData) -> Option<String> {
    msg.model
        .as_ref()
        .and_then(|m| m.model_id.clone())
        .or_else(|| msg.model.as_ref().and_then(|m| m.provider_id.clone()))
        .or_else(|| msg.model_id.clone())
        .or_else(|| msg.provider_id.clone())
        .filter(|s| !s.trim().is_empty())
}

/// A message's own time: created, else completed.
fn msg_time(msg: &MessageData) -> Option<DateTime<Utc>> {
    ms_time(msg.created_ms).or_else(|| ms_time(msg.completed_ms))
}

/// A part's own time: its start, else its end.
fn part_time(part: &PartData) -> Option<DateTime<Utc>> {
    match part {
        PartData::Reasoning { time, .. } => time
            .as_ref()
            .and_then(|t| ms_time(t.start).or_else(|| ms_time(t.end))),
        PartData::Tool { state, .. } => state
            .as_ref()
            .and_then(|s| s.time.as_ref())
            .and_then(|t| ms_time(t.start).or_else(|| ms_time(t.end))),
        _ => None,
    }
}

/// Facts stated by one part, given its message for role and fallback time.
/// The owner is always the root: subagents are separate sessions.
fn part_facts(msg: &MessageData, part: &PartData, cwd: Option<&str>) -> Vec<Fact> {
    let by = |kind| Fact {
        agent: Some(MAIN_ID.to_string()),
        ts: None,
        kind,
    };
    let mut out = Vec::new();
    match part {
        PartData::Text { text } if !text.trim().is_empty() => {
            if msg.role == "user" {
                out.push(by(FactKind::Prompt(text.clone())));
            } else {
                out.push(by(FactKind::Label {
                    agent_type: None,
                    description: Some(text.clone()),
                }));
            }
        }
        PartData::Reasoning { text, .. } if !text.trim().is_empty() => {
            out.push(by(FactKind::Reasoning(text.clone())));
        }
        PartData::Tool {
            tool,
            call_id,
            state,
            error,
        } => {
            let Some(call) = call_id else {
                return out;
            };
            let start = state
                .as_ref()
                .and_then(|s| s.time.as_ref())
                .and_then(|t| ms_time(t.start));
            let end = state
                .as_ref()
                .and_then(|s| s.time.as_ref())
                .and_then(|t| ms_time(t.end));
            out.push(Fact {
                agent: Some(MAIN_ID.to_string()),
                ts: start,
                kind: FactKind::ToolStart {
                    call: call.clone(),
                    name: tool.clone(),
                    summary: summarize_tool(tool, state.as_ref(), cwd),
                },
            });
            if is_spawn_tool(tool) {
                // The child runs in its own session (this session is its
                // `parent_id`): no synthetic node is stated, only provenance
                // for whenever that session is opened.
                out.push(Fact {
                    agent: Some(MAIN_ID.to_string()),
                    ts: start,
                    kind: FactKind::Spawn { call: call.clone() },
                });
            }
            // A part without a terminal status is still running: its call
            // stays pending, which is ground truth that the agent is working.
            let failed = error.is_some()
                || state.as_ref().and_then(|s| s.status.as_deref()) == Some("error");
            let done = state.as_ref().and_then(|s| s.status.as_deref()) == Some("completed");
            if failed || done {
                out.push(Fact {
                    agent: Some(MAIN_ID.to_string()),
                    ts: end.or(start),
                    kind: FactKind::ToolEnd {
                        call: call.clone(),
                        outcome: if failed { Outcome::Err } else { Outcome::Ok },
                    },
                });
            }
        }
        PartData::StepFinish { tokens, .. } => {
            // The turn's output: `dedup` is `None`, so every step sums its
            // own delta. Message-level `tokens` are never stated alongside —
            // they count the same output twice.
            if let Some(output) = tokens.as_ref().and_then(|t| t.output)
                && output > 0
            {
                out.push(Fact {
                    agent: Some(MAIN_ID.to_string()),
                    ts: ms_time(msg.completed_ms).or_else(|| msg_time(msg)),
                    kind: FactKind::Tokens {
                        output,
                        dedup: None,
                    },
                });
            }
            out.push(by(FactKind::Tally("steps".into())));
        }
        // `step-start` is a boundary marker, not a fact; `file` parts are
        // attachments and `patch` parts duplicate the edits the tools
        // already stated. Unknown shapes state nothing.
        _ => {}
    }
    // A part is its owner's activity even when it states nothing else —
    // except a part that states nothing at all yields no statement.
    if !out.is_empty() {
        ensure_activity(&mut out);
    }
    out
}

/// A statement is its owner's activity even when it states nothing else.
fn ensure_activity(out: &mut Vec<Fact>) {
    if !out.iter().any(|f| f.agent.as_deref() == Some(MAIN_ID)) {
        out.push(Fact {
            agent: Some(MAIN_ID.to_string()),
            ts: None,
            kind: FactKind::Activity,
        });
    }
}

/// One line for a tool part: the tool's own display title, else the field
/// that names what it did. Defensive: any shape that doesn't match yields
/// `None`.
fn summarize_tool(
    name: &str,
    state: Option<&wire::ToolState>,
    cwd: Option<&str>,
) -> Option<String> {
    let input = state?.input.as_ref()?;
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
    let summary = match name {
        "bash" => pick("command").or_else(|| pick("description")),
        "read" | "edit" | "write" => pick_path("filePath")
            .or_else(|| pick_path("path"))
            .or_else(|| pick("description")),
        "glob" | "grep" => pick("pattern").or_else(|| pick("description")),
        "webfetch" => pick("url"),
        "websearch" => pick("query"),
        "task" => pick("description"),
        _ => pick("description"),
    };
    summary.or_else(|| state?.title.clone().map(|t| truncate_summary(&t)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str) -> MessageData {
        MessageData {
            role: role.into(),
            agent: Some("build".into()),
            model: None,
            model_id: None,
            provider_id: None,
            created_ms: Some(1_000),
            completed_ms: Some(2_000),
        }
    }

    #[test]
    fn user_text_is_a_prompt_and_assistant_text_a_label() {
        let mut s = Stream::new();
        let st = s
            .push_row(&msg("user"), "p1", &PartData::Text { text: "hi".into() })
            .unwrap();
        assert!(matches!(&st.facts[1], Fact { kind: FactKind::Prompt(t), .. } if t == "hi"));
        let st = s
            .push_row(
                &msg("assistant"),
                "p2",
                &PartData::Text {
                    text: "hello".into(),
                },
            )
            .unwrap();
        assert!(matches!(
            &st.facts[0],
            Fact { kind: FactKind::Label { description: Some(t), .. }, .. } if t == "hello"
        ));
    }

    #[test]
    fn reasoning_and_tool_round_trip() {
        let mut s = Stream::new();
        let st = s
            .push_row(
                &msg("assistant"),
                "p1",
                &PartData::Reasoning {
                    text: "thinking".into(),
                    time: None,
                },
            )
            .unwrap();
        assert!(
            matches!(&st.facts[1], Fact { kind: FactKind::Reasoning(t), .. } if t == "thinking")
        );
        // A completed tool states start and end at their own times.
        let tool = PartData::Tool {
            tool: "bash".into(),
            call_id: Some("c1".into()),
            state: Some(wire::ToolState {
                status: Some("completed".into()),
                input: Some(serde_json::json!({"command": "cargo test"})),
                title: None,
                time: Some(wire::PartTime {
                    start: Some(1_000),
                    end: Some(1_500),
                }),
            }),
            error: None,
        };
        let st = s.push_row(&msg("assistant"), "p2", &tool).unwrap();
        assert!(matches!(
            &st.facts[0],
            Fact {
                kind: FactKind::ToolStart {
                    summary: Some(_),
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            &st.facts[1],
            Fact {
                kind: FactKind::ToolEnd {
                    outcome: Outcome::Ok,
                    ..
                },
                ..
            }
        ));
        assert!(st.facts[0].ts < st.facts[1].ts, "start before end");
        // A failed tool ends Err, from the status or the top-level error.
        let mut failed = tool.clone();
        if let PartData::Tool { state, .. } = &mut failed {
            state.as_mut().unwrap().status = Some("error".into());
        }
        let st = s.push_row(&msg("assistant"), "p3", &failed).unwrap();
        assert!(matches!(
            &st.facts[1],
            Fact {
                kind: FactKind::ToolEnd {
                    outcome: Outcome::Err,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn running_tools_stay_pending_and_task_spawns() {
        let mut s = Stream::new();
        // No terminal status: start only, no end — the call stays pending.
        let running = PartData::Tool {
            tool: "bash".into(),
            call_id: Some("c9".into()),
            state: Some(wire::ToolState {
                status: Some("running".into()),
                input: None,
                title: None,
                time: None,
            }),
            error: None,
        };
        let st = s.push_row(&msg("assistant"), "p1", &running).unwrap();
        assert_eq!(st.facts.len(), 2, "birth + start, no end");
        // A task spawns but states no synthetic child: the child is its own
        // session, opened by id.
        let task = PartData::Tool {
            tool: "task".into(),
            call_id: Some("ct".into()),
            state: Some(wire::ToolState {
                status: Some("completed".into()),
                input: Some(serde_json::json!({"description": "explore"})),
                title: None,
                time: None,
            }),
            error: None,
        };
        let st = s.push_row(&msg("assistant"), "p2", &task).unwrap();
        assert!(
            st.facts
                .iter()
                .any(|f| matches!(f.kind, FactKind::Spawn { .. })),
            "spawn is stated"
        );
        assert!(
            !st.facts.iter().any(|f| matches!(
                f.kind,
                FactKind::Agent { .. } if f.agent.as_deref() != Some(MAIN_ID)
            )),
            "no synthetic child node"
        );
    }

    #[test]
    fn in_place_update_re_states_while_identical_repush_is_silent() {
        use crate::fact::FactKind;
        let mut s = Stream::new();
        let running = PartData::Tool {
            tool: "bash".into(),
            call_id: Some("c1".into()),
            state: Some(wire::ToolState {
                status: Some("running".into()),
                input: Some(serde_json::json!({"command": "cargo test"})),
                title: None,
                time: Some(wire::PartTime {
                    start: Some(1_000),
                    end: None,
                }),
            }),
            error: None,
        };
        let st = s.push_row(&msg("assistant"), "p1", &running).unwrap();
        assert!(
            !st.facts
                .iter()
                .any(|f| matches!(f.kind, FactKind::ToolEnd { .. })),
            "running states no end"
        );
        // The same row UPDATEs in place to completed: the ToolEnd must be
        // stated, not swallowed by the seen-set.
        let mut done = running.clone();
        if let PartData::Tool { state, .. } = &mut done {
            let st = state.as_mut().unwrap();
            st.status = Some("completed".into());
            st.time.as_mut().unwrap().end = Some(1_500);
        }
        let st = s
            .push_row(&msg("assistant"), "p1", &done)
            .expect("mutated part re-states");
        assert!(
            st.facts
                .iter()
                .any(|f| matches!(f.kind, FactKind::ToolEnd { .. })),
            "completed re-push states the end"
        );
        // Byte-identical re-push: silent.
        assert!(
            s.push_row(&msg("assistant"), "p1", &done).is_none(),
            "identical re-push states nothing"
        );
    }

    #[test]
    fn unknown_tool_names_stay_plain_tool_starts() {
        let mut s = Stream::new();
        assert!(!is_spawn_tool("frobnicate"));
        assert!(!is_spawn_tool("Task"), "exact match: no case folding");
        let novel = PartData::Tool {
            tool: "frobnicate".into(),
            call_id: Some("cn".into()),
            state: Some(wire::ToolState {
                status: Some("completed".into()),
                input: None,
                title: None,
                time: None,
            }),
            error: None,
        };
        let st = s.push_row(&msg("assistant"), "p9", &novel).unwrap();
        assert!(
            st.facts
                .iter()
                .any(|f| matches!(f.kind, FactKind::ToolStart { .. })),
            "a novel tool still states its start"
        );
        assert!(
            !st.facts
                .iter()
                .any(|f| matches!(f.kind, FactKind::Spawn { .. })),
            "but never a Spawn for an unknown tool"
        );
    }

    #[test]
    fn step_finish_states_tokens_and_a_step_tally() {
        let mut s = Stream::new();
        let st = s
            .push_row(
                &msg("assistant"),
                "p1",
                &PartData::StepFinish {
                    tokens: Some(wire::StepTokens { output: Some(10) }),
                    reason: Some("stop".into()),
                },
            )
            .unwrap();
        assert!(st.facts.iter().any(|f| matches!(
            f.kind,
            FactKind::Tokens {
                output: 10,
                dedup: None
            }
        )));
        assert!(
            st.facts
                .iter()
                .any(|f| matches!(f.kind, FactKind::Tally(_)))
        );
        assert!(
            s.push_row(&msg("assistant"), "p1", &PartData::StepStart)
                .is_none()
        );
    }

    #[test]
    fn repeats_and_empties_state_nothing() {
        let mut s = Stream::new();
        let part = PartData::Text { text: "hi".into() };
        assert!(s.push_row(&msg("user"), "p1", &part).is_some());
        assert!(s.push_row(&msg("user"), "p1", &part).is_none(), "seen id");
        assert!(
            s.push_row(&msg("user"), "p2", &PartData::Text { text: "  ".into() })
                .is_none(),
            "blank text"
        );
        assert!(
            s.push_row(&msg("user"), "p3", &PartData::Other).is_none(),
            "unknown shapes"
        );
        assert!(s.push("garbage").is_none());
    }

    #[test]
    fn announce_states_root_title_and_cwd_once() {
        let mut s = Stream::new();
        let meta = SessionMeta {
            id: "s1".into(),
            title: "Greeting".into(),
            directory: "/proj".into(),
            agent: Some("build".into()),
            time_created: 1_000,
        };
        let st = s.announce(&meta).unwrap();
        assert!(matches!(
            &st.facts[0].kind,
            FactKind::Agent { kind: AgentKind::Main, agent_type: Some(t), .. } if t == "build"
        ));
        assert!(
            st.facts
                .iter()
                .any(|f| matches!(f.kind, FactKind::Title(_)))
        );
        assert!(s.announce(&meta).is_none(), "re-announce is nothing");
    }

    /// A pseudo-path from `all_paths` (`<db>/<session-id>`) names no file on
    /// disk, yet `open` must resolve it: it is what the sweep hands back.
    #[cfg(feature = "native")]
    #[test]
    fn open_resolves_a_pseudo_path_from_all_paths() {
        use crate::provider::{Provider, Scope, Target, open};

        let (_guard, home, _saved) = discovery::support::scratch_home("pseudo");
        let db = discovery::db_path(&home);
        discovery::support::seed_session(&db, "ses_a", "/proj", 1000);
        let pseudos = Provider::Opencode.all_paths(&Scope::ALL);
        assert_eq!(pseudos, vec![db.join("ses_a")]);
        let session = open(&Target::Path(pseudos[0].clone()), None).unwrap();
        assert_eq!(session.provider, Provider::Opencode);
        assert_eq!(session.id, "ses_a");
        assert_eq!(session.root.path, db);
        // A bare database still opens as its newest session.
        let bare = open(&Target::Path(db.clone()), None).unwrap();
        assert_eq!(bare.id, "ses_a");
    }

    /// The replay feeder reads the database, not lines: opening the scratch
    /// database builds dated items, and a second build through the adopted
    /// stream states nothing new.
    #[cfg(feature = "native")]
    #[test]
    fn replay_builds_items_from_the_database() {
        use crate::provider::{Target, open};

        let (_guard, home, _saved) = discovery::support::scratch_home("replay");
        let db = discovery::db_path(&home);
        discovery::support::seed_session(&db, "ses_1", "/proj", 1000);
        let session = open(&Target::Path(db.clone()), None).unwrap();
        assert_eq!(session.provider, crate::provider::Provider::Opencode);
        assert_eq!(session.id, "ses_1");
        let (items, _info, seed) = crate::tailer::build_replay(&session);
        assert!(!items.is_empty(), "the prompt is an item");
        assert!(items.iter().all(|i| i.ts().is_some()));
        // The follow-up tail adopts the bulk stream: a re-read is silent.
        let mut stream = seed.streams.get(&db).cloned().unwrap();
        assert!(
            crate::provider::Provider::Opencode
                .db_statements(&session.root, &mut stream)
                .is_empty()
        );
    }

    /// The shipped demo session, exported as one row per line and stated
    /// through the line feed, run through the whole conformance check: the
    /// model and timeline goldens beside the fixture.
    #[test]
    fn demo_session_conforms() {
        let Some(dir) = crate::provider::harness::fixture_dir("opencode") else {
            return;
        };
        let text = std::fs::read_to_string(dir.join("demo.jsonl")).unwrap();
        let streams = || {
            let mut stream = Stream::new();
            vec![
                text.lines()
                    .filter_map(|l| stream.push(l))
                    .collect::<Vec<_>>(),
            ]
        };
        crate::provider::harness::conform("opencode", "demo", streams);
    }
}
