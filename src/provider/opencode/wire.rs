//! Opencode's wire format: the serde model for one SQLite row's `data`
//! column, and for one line of an exported session (`assets/opencode/`).
//!
//! The live store is three tables in `~/.local/share/opencode/opencode.db`:
//! `session(id, directory, title, agent, time_created, ...)`,
//! `message(id, session_id, time_*, data)` and
//! `part(id, message_id, session_id, time_*, data)`. Timestamps are epoch
//! millis. `message.data` carries `{role, agent, model|modelID, time,
//! tokens, ...}`; `part.data` is tagged by `type`: `text`, `reasoning`,
//! `tool`, `step-start`, `step-finish`, `file`, `patch` (verified against the
//! live database; anything else parses as [`PartData::Other`]).
//!
//! Defensive by design: unknown part types, missing fields and malformed rows
//! parse to something skippable, never a panic. What the rows *mean* is the
//! provider's job ([`super`]); where the database *lives* is
//! [`super::discovery`].

use chrono::{DateTime, Utc};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Row envelopes
// ---------------------------------------------------------------------------

/// One row of the `session` table, as the feeder reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub directory: String,
    pub agent: Option<String>,
    pub time_created: i64,
}

/// One row of the `message` table: its identity plus the parsed `data`.
#[derive(Debug, Clone, PartialEq)]
pub struct MessageData {
    pub role: String,
    pub agent: Option<String>,
    /// User messages carry `model: {providerID, modelID}`.
    pub model: Option<ModelRef>,
    /// Assistant messages carry top-level `modelID` / `providerID` strings.
    pub model_id: Option<String>,
    pub provider_id: Option<String>,
    pub created_ms: Option<i64>,
    pub completed_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ModelRef {
    #[serde(rename = "providerID")]
    pub provider_id: Option<String>,
    #[serde(rename = "modelID")]
    pub model_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
struct MsgTime {
    created: Option<i64>,
    completed: Option<i64>,
}

/// The serde shape of a message's `data` column: every field optional so a
/// newer or thinner row still parses. Converted to [`MessageData`] on read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RawMessage {
    #[serde(default)]
    role: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<ModelRef>,
    #[serde(default, rename = "modelID")]
    model_id: Option<String>,
    #[serde(default, rename = "providerID")]
    provider_id: Option<String>,
    #[serde(default)]
    time: Option<MsgTime>,
}

/// Parse a `message` row's `data` column. `None` when it is not JSON or not
/// an object; an unknown or missing role parses through and states nothing.
///
/// Native-only in production (only the SQLite reader calls it) but used by
/// the portable unit tests, hence `any(feature = "native", test)`: without
/// it `cargo check --no-default-features` warns it is never used.
#[cfg(any(feature = "native", test))]
pub fn parse_message(data: &str) -> Option<MessageData> {
    let raw: RawMessage = serde_json::from_str(data).ok()?;
    Some(MessageData {
        role: raw.role,
        agent: raw.agent,
        model: raw.model,
        model_id: raw.model_id,
        provider_id: raw.provider_id,
        created_ms: raw.time.as_ref().and_then(|t| t.created),
        completed_ms: raw.time.as_ref().and_then(|t| t.completed),
    })
}

// ---------------------------------------------------------------------------
// Parts
// ---------------------------------------------------------------------------

/// A `part` row's `data` `time` object: when the part ran.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PartTime {
    pub start: Option<i64>,
    pub end: Option<i64>,
}

/// What a `tool` part's `state` says: its status, input, and timing.
/// `output` and `metadata` are deliberately untyped: the provider states only
/// the outcome and a one-line summary, never the tool's payload.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ToolState {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub time: Option<PartTime>,
}

/// Token counts on a `step-finish` part. Only `output` is stated; the rest
/// documents the shape without feeding the fold.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct StepTokens {
    #[serde(default)]
    pub output: Option<u64>,
}

/// One `part` row's `data`, by its `type`. `File` and `Patch` are named (and
/// skipped by the provider) so the skip is a reviewed decision, not an
/// accident; anything newer is [`PartData::Other`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PartData {
    Text {
        #[serde(default)]
        text: String,
    },
    Reasoning {
        #[serde(default)]
        text: String,
        #[serde(default)]
        time: Option<PartTime>,
    },
    Tool {
        #[serde(default)]
        tool: String,
        #[serde(default, rename = "callID")]
        call_id: Option<String>,
        #[serde(default)]
        state: Option<ToolState>,
        /// A top-level failure note (e.g. `"Task cancelled"`); the usual
        /// shape of a failed `task` part.
        #[serde(default)]
        error: Option<String>,
    },
    #[serde(rename = "step-start")]
    StepStart,
    #[serde(rename = "step-finish")]
    StepFinish {
        #[serde(default)]
        tokens: Option<StepTokens>,
        #[serde(default)]
        reason: Option<String>,
    },
    File {
        #[serde(default)]
        filename: Option<String>,
    },
    Patch {
        #[serde(default)]
        files: Vec<String>,
    },
    #[serde(other)]
    Other,
}

/// Parse a `part` row's `data` column. `None` for malformed JSON; unknown
/// shapes become [`PartData::Other`] via the serde tag above. Same
/// native-or-test gating as [`parse_message`].
#[cfg(any(feature = "native", test))]
pub fn parse_part(data: &str) -> Option<PartData> {
    serde_json::from_str(data).ok()
}

// ---------------------------------------------------------------------------
// Exported lines (fixtures and transfers)
// ---------------------------------------------------------------------------

/// A session's messages with their parts, in time order: `(message id,
/// message, [(part id, part)])`. Message ids are kept so the stream's
/// seen-set makes re-reads idempotent.
pub type SessionRows = Vec<(String, MessageData, Vec<(String, PartData)>)>;

/// One line of an exported session: `assets/opencode/demo.jsonl` is this
/// shape, one row per line, sanitized. A `part` line embeds its message so a
/// line states on its own, with no cross-line context to lose.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "table", rename_all = "lowercase")]
pub enum ExportRow {
    Session(SessionMetaRow),
    Message(MessageRow),
    // Boxed: the part line embeds its message, dwarfing the other rows.
    Part(Box<PartRow>),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SessionMetaRow {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub directory: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub time_created: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MessageRow {
    pub id: String,
    pub session_id: String,
    #[serde(default)]
    pub time_created: i64,
    pub data: RawMessage,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PartRow {
    pub id: String,
    pub message_id: String,
    pub session_id: String,
    #[serde(default)]
    pub time_created: i64,
    pub message: RawMessage,
    pub data: PartData,
}

/// Parse one exported line. `None` for blank or unparsable lines.
pub fn parse_export_row(line: &str) -> Option<ExportRow> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

impl From<&SessionMetaRow> for SessionMeta {
    fn from(r: &SessionMetaRow) -> Self {
        SessionMeta {
            id: r.id.clone(),
            title: r.title.clone(),
            directory: r.directory.clone(),
            agent: r.agent.clone(),
            time_created: r.time_created,
        }
    }
}

impl From<&RawMessage> for MessageData {
    fn from(r: &RawMessage) -> Self {
        MessageData {
            role: r.role.clone(),
            agent: r.agent.clone(),
            model: r.model.clone(),
            model_id: r.model_id.clone(),
            provider_id: r.provider_id.clone(),
            created_ms: r.time.as_ref().and_then(|t| t.created),
            completed_ms: r.time.as_ref().and_then(|t| t.completed),
        }
    }
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// Epoch millis to UTC. `None` for missing or out-of-range values: a bad
/// clock states nothing rather than panicking the fold.
pub(crate) fn ms_time(ms: Option<i64>) -> Option<DateTime<Utc>> {
    ms.and_then(DateTime::from_timestamp_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_carries_nested_model() {
        let m = parse_message(
            r#"{"role":"user","agent":"build","model":{"providerID":"opencode","modelID":"big-pickle"},"time":{"created":1783372446793}}"#,
        )
        .unwrap();
        assert_eq!(m.role, "user");
        assert_eq!(m.agent.as_deref(), Some("build"));
        assert_eq!(
            m.model.as_ref().and_then(|x| x.model_id.clone()).as_deref(),
            Some("big-pickle")
        );
        assert!(ms_time(m.created_ms).is_some());
    }

    #[test]
    fn assistant_message_carries_flat_model_and_tokens() {
        // `tokens`, `finish`, `cost` parse through and are ignored by the
        // provider: usage is stated from `step-finish` parts, and a turn's
        // stop reason is never an agent's ending.
        let m = parse_message(
            r#"{"role":"assistant","agent":"build","modelID":"big-pickle","providerID":"opencode","finish":"stop","tokens":{"input":1,"output":2},"time":{"created":1783372446816,"completed":1783372449291}}"#,
        )
        .unwrap();
        assert_eq!(m.model_id.as_deref(), Some("big-pickle"));
        assert_eq!(m.completed_ms, Some(1783372449291));
    }

    #[test]
    fn malformed_message_is_nothing() {
        assert!(parse_message("not json").is_none());
        // Valid JSON, wrong shape: no object to read a role from.
        assert!(parse_message("[1,2]").is_none());
    }

    #[test]
    fn tool_part_shapes() {
        let PartData::Tool {
            tool,
            call_id,
            state,
            error,
        } = parse_part(
            r#"{"type":"tool","tool":"read","callID":"call_1","state":{"status":"completed","input":{"filePath":"/x"},"title":"x","time":{"start":10,"end":12}}}"#,
        )
        .unwrap()
        else {
            panic!("tool part");
        };
        assert_eq!(tool, "read");
        assert_eq!(call_id.as_deref(), Some("call_1"));
        assert_eq!(
            state.as_ref().and_then(|s| s.status.clone()).as_deref(),
            Some("completed")
        );
        assert!(error.is_none());
        // A cancelled task carries its failure at the top level.
        let PartData::Tool { state, error, .. } = parse_part(
            r#"{"type":"tool","tool":"task","callID":"c","state":{"status":"error","input":{}},"error":"Task cancelled"}"#,
        )
        .unwrap()
        else {
            panic!("task part");
        };
        assert!(error.is_some());
        assert_eq!(
            state.as_ref().and_then(|s| s.status.clone()).as_deref(),
            Some("error")
        );
    }

    #[test]
    fn every_known_part_type_parses() {
        assert!(matches!(
            parse_part(r#"{"type":"text","text":"hi"}"#).unwrap(),
            PartData::Text { .. }
        ));
        assert!(matches!(
            parse_part(r#"{"type":"reasoning","text":"hmm"}"#).unwrap(),
            PartData::Reasoning { .. }
        ));
        assert!(matches!(
            parse_part(r#"{"type":"step-start"}"#).unwrap(),
            PartData::StepStart
        ));
        assert!(matches!(
            parse_part(r#"{"type":"step-finish","tokens":{"output":10},"reason":"stop"}"#).unwrap(),
            PartData::StepFinish { .. }
        ));
        assert!(matches!(
            parse_part(r#"{"type":"file","mime":"text/plain","filename":"a"}"#).unwrap(),
            PartData::File { .. }
        ));
        assert!(matches!(
            parse_part(r#"{"type":"patch","files":["/a"]}"#).unwrap(),
            PartData::Patch { .. }
        ));
        // Newer than this build: skipped, never a panic.
        assert!(matches!(
            parse_part(r#"{"type":"hologram","x":1}"#).unwrap(),
            PartData::Other
        ));
        assert!(parse_part("{oops").is_none());
    }

    #[test]
    fn export_rows_round_trip() {
        let row = parse_export_row(
            r#"{"table":"part","id":"p1","message_id":"m1","session_id":"s1","message":{"role":"assistant"},"data":{"type":"text","text":"hi"}}"#,
        )
        .unwrap();
        assert!(matches!(row, ExportRow::Part(_)));
        assert!(parse_export_row("").is_none());
        assert!(parse_export_row("garbage").is_none());
    }

    #[test]
    fn ms_time_rejects_garbage() {
        assert!(ms_time(None).is_none());
        assert!(ms_time(Some(i64::MAX)).is_none());
        assert!(ms_time(Some(1783372446793)).is_some());
    }
}
