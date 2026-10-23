//! jcode's wire format: one JSON document per session, rewritten on every
//! turn. Nothing here knows what the records mean ([`super`]) or where the
//! files live ([`super::discovery`]).
//!
//! Defensive by design: the file is rewritten while it is read, so a torn
//! write must parse to `None` (retried on the next tick), never a panic. One
//! corrupt message must not sink the document either: messages arrive as raw
//! values and each parses on its own, so a bad one is skipped and the rest
//! still state their facts.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

/// The whole session document. Built by [`parse_doc`], never deserialized
/// directly: `messages` arrive as raw values so one corrupt entry cannot fail
/// the hundreds around it.
#[derive(Debug, Clone, Default)]
pub struct Document {
    pub id: Option<String>,
    pub working_dir: Option<String>,
    pub model: Option<String>,
    pub provider_key: Option<String>,
    pub title: Option<String>,
    pub messages: Vec<Message>,
}

/// The top-level shape, with messages still raw.
#[derive(Debug, Deserialize)]
struct RawDocument {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider_key: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    messages: Vec<serde_json::Value>,
}

/// One turn's record in the document.
///
/// `role` is `"user"` or `"assistant"` on every record observed live.
/// `display_role` refines a user record: `"system"` for the injected session
/// context (a `<system-reminder>`, not a prompt) and `"background_task"` for
/// a background worker's completion notice.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub display_role: Option<String>,
    #[serde(default)]
    pub content: Vec<Block>,
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
}

/// One content block. `tool_use` rides on assistant records, `tool_result` on
/// user records, joined by the call id. Unknown block types fall through to
/// [`Block::Unknown`] so a newer jcode never breaks parsing.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum Block {
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "reasoning")]
    Reasoning {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "tool_use")]
    ToolUse(ToolUse),
    #[serde(rename = "tool_result")]
    ToolResult(ToolResult),
    #[serde(other)]
    Unknown,
}

/// A `tool_use` block: what the agent ran, with its raw input.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolUse {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Raw tool input; shape varies by tool. Every tool observed carries an
    /// `intent` string naming why it ran (see [`super::summarize_tool`]).
    #[serde(default)]
    pub input: serde_json::Value,
}

/// A `tool_result` block. Pairs with a prior `tool_use` via `tool_use_id`.
/// The result payload (`content`) is not needed for the graph — only whether
/// the call failed — so it is not modelled.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolResult {
    #[serde(rename = "tool_use_id", default)]
    pub tool_use_id: Option<String>,
    /// **Missing means success.** Only `Some(true)` indicates failure — the
    /// same rule as Claude's format, and the dominant shape live (most
    /// results omit the flag while failures carry it).
    #[serde(default)]
    pub is_error: Option<bool>,
}

// ---------------------------------------------------------------------------
// Parsing entry point
// ---------------------------------------------------------------------------

/// Parse a whole session document. Returns `None` on any failure — a torn
/// mid-write read, garbage — never panics. One bad message is skipped; the
/// document still parses around it.
pub fn parse_doc(text: &str) -> Option<Document> {
    let raw: RawDocument = serde_json::from_str(text.trim()).ok()?;
    let messages = raw
        .messages
        .into_iter()
        .filter_map(|m| serde_json::from_value::<Message>(m).ok())
        .collect();
    Some(Document {
        id: raw.id,
        working_dir: raw.working_dir,
        model: raw.model,
        provider_key: raw.provider_key,
        title: raw.title,
        messages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Document {
        parse_doc(
            r#"{
                "id": "session_demo_1_a",
                "working_dir": "/project",
                "model": "test-model",
                "provider_key": "test",
                "title": "Demo",
                "messages": [
                    {"id": "m1", "role": "user", "timestamp": "2026-09-01T10:00:00.000Z",
                     "content": [{"type": "text", "text": "map the auth flow"}]},
                    {"id": "m2", "role": "assistant", "timestamp": "2026-09-01T10:00:05.000Z",
                     "content": [{"type": "reasoning", "text": "looking"},
                                 {"type": "tool_use", "id": "call_1", "name": "bash",
                                  "input": {"command": "ls", "intent": "list files"}}]},
                    {"id": "m3", "role": "user", "timestamp": "2026-09-01T10:00:06.000Z",
                     "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "a.rs"}]}
                ]
            }"#,
        )
        .expect("doc parses")
    }

    #[test]
    fn document_and_message_shapes() {
        let d = doc();
        assert_eq!(d.id.as_deref(), Some("session_demo_1_a"));
        assert_eq!(d.working_dir.as_deref(), Some("/project"));
        assert_eq!(d.model.as_deref(), Some("test-model"));
        assert_eq!(d.messages.len(), 3);
        assert!(d.messages[0].timestamp.is_some());
        let [Block::ToolUse(tu), ..] = &d.messages[1].content[1..] else {
            panic!("second block is tool_use");
        };
        assert_eq!(tu.id.as_deref(), Some("call_1"));
        assert_eq!(tu.name.as_deref(), Some("bash"));
        // `is_error` absent → None (treated as success downstream).
        let Block::ToolResult(tr) = &d.messages[2].content[0] else {
            panic!("tool_result");
        };
        assert_eq!(tr.is_error, None);
    }

    #[test]
    fn unknown_block_type_and_missing_fields() {
        let d = parse_doc(
            r#"{"id": "s", "messages": [
                {"id": "m1", "role": "assistant", "content": [{"type": "future-block", "x": 1}]},
                {"id": "m2", "role": "assistant", "content": [{"type": "text"}]}
            ]}"#,
        )
        .expect("parses");
        assert!(matches!(d.messages[0].content[0], Block::Unknown));
        // A missing `text` defaults, it does not fail the block.
        assert!(matches!(
            &d.messages[1].content[0],
            Block::Text { text } if text.is_empty()
        ));
    }

    #[test]
    fn one_bad_message_does_not_sink_the_document() {
        // `content` as a bare string fails `Vec<Block>`; the message is
        // skipped, its neighbours survive.
        let d = parse_doc(
            r#"{"id": "s", "messages": [
                {"id": "good-1", "role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"id": "bad", "role": "user", "content": "not-an-array"},
                {"id": "good-2", "role": "user", "content": [{"type": "text", "text": "yo"}]}
            ]}"#,
        )
        .expect("parses");
        let ids: Vec<_> = d.messages.iter().filter_map(|m| m.id.as_deref()).collect();
        assert_eq!(ids, ["good-1", "good-2"]);
    }

    #[test]
    fn garbage_blank_and_missing_messages_return_none_or_empty() {
        assert!(parse_doc("not json at all").is_none());
        assert!(parse_doc("{ broken json").is_none());
        assert!(parse_doc("").is_none());
        assert!(parse_doc("   ").is_none());
        // A torn mid-write read is not a document.
        assert!(parse_doc(r#"{"id": "s", "messages": [{"id": "#).is_none());
        // No messages key at all: an empty document, not a failure.
        let d = parse_doc(r#"{"id": "s"}"#).expect("parses");
        assert!(d.messages.is_empty());
    }
}
