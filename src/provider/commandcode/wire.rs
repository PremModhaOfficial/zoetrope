//! Command Code's wire format: the serde model for one JSONL line, and
//! nothing else. What the records *mean* is the provider's job ([`super`]);
//! where the files *live* is [`super::discovery`].
//!
//! Defensive by design: unknown entry types, missing fields and malformed
//! lines parse to something skippable, never a panic.

use chrono::{DateTime, Utc};
use serde::Deserialize;

/// The session header: the first line of every file.
#[derive(Debug, Clone, Deserialize)]
pub struct Header {
    /// The writer's format version. Kept as a raw value so a shape we do not
    /// recognize (a string, a null) still parses and is *rejected* by
    /// `version_number`, instead of failing the whole line.
    #[serde(default)]
    pub version: Option<serde_json::Value>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub cwd: Option<String>,
}

impl Header {
    /// The version as the format states it: a number. Anything else (absent,
    /// null, a string) is stated-but-not-numeric and does not qualify.
    pub fn version_number(&self) -> Option<u64> {
        self.version.as_ref()?.as_u64()
    }
}

/// The envelope every non-header entry shares. `id`/`parentId` form the
/// writer's edit tree; nothing consumes it (the vocabulary has no branch or
/// tree fact), but it is parsed so the shape is total.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Envelope {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "parentId", default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub timestamp: Option<DateTime<Utc>>,
}

/// One parsed JSONL line, tagged on `type`. The tag is consumed by the enum;
/// no variant struct declares a `type` field.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum Entry {
    #[serde(rename = "session")]
    Session(Header),
    #[serde(rename = "message")]
    Message(MessageEntry),
    #[serde(rename = "compaction")]
    Compaction(Compaction),
    #[serde(rename = "model_change")]
    ModelChange(ModelChange),
    #[serde(rename = "effort_change")]
    EffortChange(EffortChange),
    #[serde(rename = "session_info")]
    SessionInfo(SessionInfo),
    /// `branch_summary`, `custom`, `custom_message`, `label` (writer-only,
    /// never observed) and anything a later version adds: always skipped.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageEntry {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(rename = "sessionId", default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub meta: Option<Meta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Meta {
    /// `user|steering|followup|tool|model|scheduled-cron:<id>` or absent.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(rename = "messageId", default)]
    pub message_id: Option<String>,
    /// Real data carries epoch millis (`"createdAt":1790876590577`), not a
    /// string; unused by the fact mapping. Raw-typed so any shape (number,
    /// string, absent) parses and never fails the line.
    #[serde(rename = "createdAt", default)]
    pub created_at: Option<serde_json::Value>,
    /// Real data carries an object here (`{"companions":[…]}`; 141
    /// occurrences), unused by the fact mapping. Raw-typed so any shape
    /// (object, bool, absent) parses and never fails the line.
    #[serde(default)]
    pub injected: Option<serde_json::Value>,
    #[serde(rename = "isAutomated", default)]
    pub is_automated: Option<bool>,
    #[serde(rename = "isMeta", default)]
    pub is_meta: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "thinking")]
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    #[serde(rename = "tool_use")]
    ToolUse(ToolUse),
    #[serde(rename = "tool_result")]
    ToolResult(ToolResult),
    #[serde(rename = "image")]
    Image(Image),
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolUse {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Raw input; shape varies by tool (see the summary lexicon, [`super`]).
    #[serde(default)]
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolResult {
    #[serde(rename = "tool_use_id", default)]
    pub tool_use_id: Option<String>,
    /// `content[]` of text/image blocks. Nothing consumes it; kept raw so any
    /// shape parses.
    #[serde(default)]
    pub content: Option<serde_json::Value>,
    /// Defensive: real data never carries it (`is_error` matched 0 times across
    /// every session). Absent or false means success; only `Some(true)` fails.
    #[serde(default)]
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Image {
    #[serde(default)]
    pub source: serde_json::Value,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Usage {
    #[serde(rename = "inputTokens", default)]
    pub input_tokens: Option<u64>,
    #[serde(rename = "outputTokens", default)]
    pub output_tokens: Option<u64>,
    #[serde(rename = "cacheReadTokens", default)]
    pub cache_read_tokens: Option<u64>,
    #[serde(rename = "cacheWriteTokens", default)]
    pub cache_write_tokens: Option<u64>,
    #[serde(rename = "costUsd", default)]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Compaction {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(rename = "firstKeptEntryId", default)]
    pub first_kept_entry_id: Option<String>,
    #[serde(rename = "tokensBefore", default)]
    pub tokens_before: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelChange {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EffortChange {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionInfo {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(default)]
    pub name: Option<String>,
}

/// Parse a single JSONL line. `None` on a blank/unparsable line — never panics.
pub fn parse_line(line: &str) -> Option<Entry> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str::<Entry>(trimmed).ok()
}

/// The header a line states, when it qualifies: `type == "session"` with a
/// numeric `version`. The single definition of "header", used by discovery.
pub fn parse_header(line: &str) -> Option<Header> {
    match parse_line(line)? {
        Entry::Session(h) if h.version_number().is_some() => Some(h),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = r#"{"type":"session","version":3,"id":"0b7aa2b4-1111-4111-8111-111111111111","timestamp":"2026-09-30T10:00:00.000Z","cwd":"/project"}"#;

    #[test]
    fn header_version_is_required_and_numeric() {
        let Entry::Session(h) = parse_line(HEADER).expect("header parses") else {
            panic!("expected a session header");
        };
        assert_eq!(
            h.id.as_deref(),
            Some("0b7aa2b4-1111-4111-8111-111111111111")
        );
        assert_eq!(h.cwd.as_deref(), Some("/project"));
        assert!(h.timestamp.is_some());
        assert_eq!(h.version_number(), Some(3));
        assert!(parse_header(HEADER).is_some());

        // A header-shaped line still parses when its version is absent, a
        // string or null; it just states no numeric version, so it does not
        // qualify as a header.
        for line in [
            r#"{"type":"session","id":"x"}"#,
            r#"{"type":"session","version":"3","id":"x"}"#,
            r#"{"type":"session","version":null,"id":"x"}"#,
        ] {
            let Entry::Session(h) = parse_line(line).unwrap_or_else(|| panic!("parses: {line}"))
            else {
                panic!("expected a session header: {line}");
            };
            assert_eq!(h.version_number(), None, "not numeric: {line}");
            assert!(parse_header(line).is_none(), "not a header: {line}");
        }
    }

    #[test]
    fn assistant_text_thinking_tool_use() {
        let line = r#"{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-09-30T10:00:09.000Z","model":"claude-sonnet-4-5","effort":"high","usage":{"inputTokens":1200,"outputTokens":64,"cacheReadTokens":0,"cacheWriteTokens":120,"costUsd":0.0038},"message":{"role":"assistant","content":[{"type":"thinking","thinking":"Thread a dry_run flag through the entry point.","signature":"c2lnLTE"},{"type":"text","text":"I will add the flag behind a boolean and extend the test."},{"type":"tool_use","id":"call_01_edit","name":"edit_file","input":{"file_path":"/project/src/exporter.rs","old_string":"pub fn run(config: Config) {","new_string":"pub fn run(config: Config, dry_run: bool) {"}}],"meta":{"source":"model","messageId":"msg-2"}}}"#;
        let Entry::Message(m) = parse_line(line).expect("assistant line parses") else {
            panic!("expected a message entry");
        };
        assert_eq!(m.envelope.id.as_deref(), Some("a2"));
        assert_eq!(m.envelope.parent_id.as_deref(), Some("a1"));
        assert!(m.envelope.timestamp.is_some());
        assert_eq!(m.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(m.effort.as_deref(), Some("high"));
        let usage = m.usage.as_ref().expect("usage");
        assert_eq!(usage.input_tokens, Some(1200));
        assert_eq!(usage.output_tokens, Some(64));
        assert_eq!(usage.cache_read_tokens, Some(0));
        assert_eq!(usage.cache_write_tokens, Some(120));
        assert_eq!(usage.cost_usd, Some(0.0038));

        let msg = m.message.as_ref().expect("message");
        assert_eq!(msg.role.as_deref(), Some("assistant"));
        assert_eq!(
            msg.meta.as_ref().and_then(|mt| mt.message_id.as_deref()),
            Some("msg-2")
        );
        assert_eq!(msg.content.len(), 3);
        assert!(matches!(
            &msg.content[0],
            ContentBlock::Thinking { thinking, signature }
                if thinking == "Thread a dry_run flag through the entry point."
                    && signature.as_deref() == Some("c2lnLTE")
        ));
        assert!(matches!(
            &msg.content[1],
            ContentBlock::Text { text }
                if text == "I will add the flag behind a boolean and extend the test."
        ));
        let ContentBlock::ToolUse(tu) = &msg.content[2] else {
            panic!("third block is a tool_use");
        };
        assert_eq!(tu.id.as_deref(), Some("call_01_edit"));
        assert_eq!(tu.name.as_deref(), Some("edit_file"));
        assert_eq!(
            tu.input.get("file_path").and_then(|v| v.as_str()),
            Some("/project/src/exporter.rs")
        );
    }

    #[test]
    fn tool_result_absent_is_error_is_success() {
        let line = r#"{"type":"message","id":"a3","parentId":"a2","timestamp":"2026-09-30T10:00:10.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_01_edit","content":[{"type":"text","text":"Edited src/exporter.rs"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}]}],"meta":{"source":"tool","messageId":"msg-3"}}}"#;
        let Entry::Message(m) = parse_line(line).expect("result line parses") else {
            panic!("expected a message entry");
        };
        let msg = m.message.as_ref().expect("message");
        let ContentBlock::ToolResult(tr) = &msg.content[0] else {
            panic!("expected a tool_result block");
        };
        assert_eq!(tr.tool_use_id.as_deref(), Some("call_01_edit"));
        assert_eq!(tr.is_error, None, "absent is_error is success");
        let content = tr.content.as_ref().expect("content is kept raw");
        assert_eq!(content.as_array().map(Vec::len), Some(2));
        assert_eq!(
            content[1].get("type").and_then(|v| v.as_str()),
            Some("image")
        );
    }

    #[test]
    fn tool_result_is_error_true() {
        let line = r#"{"type":"message","id":"a4","timestamp":"2026-09-30T10:00:11.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_02_fail","content":[{"type":"text","text":"boom"}],"is_error":true}],"meta":{"source":"tool","messageId":"msg-4"}}}"#;
        let Entry::Message(m) = parse_line(line).expect("result line parses") else {
            panic!("expected a message entry");
        };
        let msg = m.message.as_ref().expect("message");
        let ContentBlock::ToolResult(tr) = &msg.content[0] else {
            panic!("expected a tool_result block");
        };
        assert_eq!(tr.is_error, Some(true));
    }

    #[test]
    fn unknown_type_skips() {
        // Writer-only types never observed in real data, and a novel type:
        // every one is skippable, never a parse failure.
        for line in [
            r#"{"type":"label","id":"a13","value":"ignored"}"#,
            r#"{"type":"custom","x":1}"#,
            r#"{"type":"custom_message","text":"x"}"#,
            r#"{"type":"branch_summary","summary":"x"}"#,
            r#"{"type":"brand-new-future-type","x":1}"#,
        ] {
            assert!(
                matches!(parse_line(line), Some(Entry::Unknown)),
                "skippable: {line}"
            );
        }
    }

    #[test]
    fn garbage_and_blank_return_none() {
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
        assert!(parse_line("\n").is_none());
        assert!(parse_line("not json").is_none());
        assert!(parse_line(r#"{"type":"message""#).is_none());
        // A `.checkpoints.jsonl` first line has no `type`: rejected for free.
        assert!(
            parse_line(r#"{"id":"c1","messageId":"m1","turnNumber":1,"prompt":"p"}"#).is_none()
        );
        // A message whose content fails to deserialize fails only its line.
        assert!(parse_line(r#"{"type":"message","message":"not an object"}"#).is_none());
    }

    #[test]
    fn meta_source_absent_and_unknown() {
        // Absent `source` parses to None — tolerated, not rejected.
        let no_source = r#"{"type":"message","id":"a7","timestamp":"2026-09-30T10:01:06.000Z","message":{"role":"user","content":[{"type":"text","text":"Error: 503 Service Unavailable"}],"meta":{"messageId":"msg-7"}}}"#;
        let Entry::Message(m) = parse_line(no_source).expect("no-source line parses") else {
            panic!("expected a message entry");
        };
        let msg = m.message.as_ref().expect("message");
        let meta = msg.meta.as_ref().expect("meta");
        assert_eq!(meta.source, None);
        assert_eq!(meta.message_id.as_deref(), Some("msg-7"));

        // A source outside the observed vocabulary is kept as stated.
        let cron = r#"{"type":"message","id":"c1","timestamp":"2026-09-30T10:01:07.000Z","message":{"role":"user","content":[{"type":"text","text":"tick"}],"meta":{"source":"scheduled-cron:job-1","isAutomated":true}}}"#;
        let Entry::Message(m) = parse_line(cron).expect("cron line parses") else {
            panic!("expected a message entry");
        };
        let meta = m
            .message
            .as_ref()
            .and_then(|msg| msg.meta.as_ref())
            .expect("meta");
        assert_eq!(meta.source.as_deref(), Some("scheduled-cron:job-1"));
        assert_eq!(meta.is_automated, Some(true));

        // A message with no meta at all parses with `meta == None`.
        let bare = r#"{"type":"message","id":"b1","timestamp":"2026-09-30T10:01:08.000Z","message":{"role":"user","content":[]}}"#;
        let Entry::Message(m) = parse_line(bare).expect("meta-less line parses") else {
            panic!("expected a message entry");
        };
        assert!(m.message.as_ref().expect("message").meta.is_none());
    }

    #[test]
    fn meta_created_at_numeric_epoch_parses() {
        // Real metas carry epoch millis as a JSON number; a typed
        // `DateTime<Utc>` failed every real message line, so any shape parses.
        let numeric = r#"{"type":"message","id":"m1","timestamp":"2026-09-30T10:00:04.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"meta":{"source":"user","messageId":"m","createdAt":1790876590577}}}"#;
        let Entry::Message(m) = parse_line(numeric).expect("numeric createdAt parses") else {
            panic!("expected a message entry");
        };
        let meta = m
            .message
            .as_ref()
            .and_then(|msg| msg.meta.as_ref())
            .expect("meta");
        assert_eq!(meta.source.as_deref(), Some("user"));
        assert!(meta.created_at.is_some(), "numeric createdAt is kept");
        assert_eq!(
            meta.created_at.as_ref().and_then(|v| v.as_u64()),
            Some(1790876590577)
        );

        // A string shape parses the same way.
        let string = r#"{"type":"message","id":"m2","timestamp":"2026-09-30T10:00:05.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"meta":{"source":"user","createdAt":"2026-09-30T10:00:05.000Z"}}}"#;
        let Entry::Message(m) = parse_line(string).expect("string createdAt parses") else {
            panic!("expected a message entry");
        };
        let meta = m
            .message
            .as_ref()
            .and_then(|msg| msg.meta.as_ref())
            .expect("meta");
        assert!(meta.created_at.is_some(), "string createdAt is kept");

        // Absent parses too, with `None`.
        let absent = r#"{"type":"message","id":"m3","timestamp":"2026-09-30T10:00:06.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"meta":{"source":"user","messageId":"m"}}}"#;
        let Entry::Message(m) = parse_line(absent).expect("absent createdAt parses") else {
            panic!("expected a message entry");
        };
        let meta = m
            .message
            .as_ref()
            .and_then(|msg| msg.meta.as_ref())
            .expect("meta");
        assert!(meta.created_at.is_none(), "absent createdAt is None");
    }

    #[test]
    fn meta_injected_object_parses() {
        // Real metas carry `injected` as an object (`{"companions":[…]}`); a
        // typed `Option<bool>` dropped every such line, so it is raw too.
        let line = r#"{"type":"message","id":"m4","timestamp":"2026-09-30T10:00:07.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"meta":{"source":"user","messageId":"m","injected":{"companions":[{"kind":"skill","text":"x"}]}}}}"#;
        let Entry::Message(m) = parse_line(line).expect("object injected parses") else {
            panic!("expected a message entry");
        };
        let meta = m
            .message
            .as_ref()
            .and_then(|msg| msg.meta.as_ref())
            .expect("meta");
        assert_eq!(meta.source.as_deref(), Some("user"));
        let injected = meta.injected.as_ref().expect("object injected is kept");
        assert_eq!(
            injected
                .get("companions")
                .and_then(|c| c.as_array())
                .map(Vec::len),
            Some(1)
        );

        // A boolean shape parses the same way.
        let bool_line = r#"{"type":"message","id":"m5","timestamp":"2026-09-30T10:00:08.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}],"meta":{"source":"scheduled-cron:job-1","injected":false}}}"#;
        let Entry::Message(m) = parse_line(bool_line).expect("bool injected parses") else {
            panic!("expected a message entry");
        };
        let meta = m
            .message
            .as_ref()
            .and_then(|msg| msg.meta.as_ref())
            .expect("meta");
        assert_eq!(
            meta.injected.as_ref().and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn compaction_fields() {
        let line = r#"{"type":"compaction","id":"a11","parentId":"a10","timestamp":"2026-09-30T10:03:00.000Z","summary":"Trimmed the fixture conversation to its load-bearing turns.","firstKeptEntryId":"a6","tokensBefore":18000}"#;
        let Entry::Compaction(c) = parse_line(line).expect("compaction parses") else {
            panic!("expected a compaction entry");
        };
        assert_eq!(c.envelope.id.as_deref(), Some("a11"));
        assert_eq!(c.envelope.parent_id.as_deref(), Some("a10"));
        assert_eq!(
            c.summary.as_deref(),
            Some("Trimmed the fixture conversation to its load-bearing turns.")
        );
        assert_eq!(c.first_kept_entry_id.as_deref(), Some("a6"));
        assert_eq!(c.tokens_before, Some(18000));
    }

    #[test]
    fn model_change_field() {
        let line = r#"{"type":"model_change","id":"a9","parentId":"a8","timestamp":"2026-09-30T10:02:00.000Z","model":"claude-opus-4-8"}"#;
        let Entry::ModelChange(mc) = parse_line(line).expect("model_change parses") else {
            panic!("expected a model_change entry");
        };
        assert_eq!(mc.envelope.id.as_deref(), Some("a9"));
        assert_eq!(mc.model.as_deref(), Some("claude-opus-4-8"));
        // A model_change without a model states nothing but still parses.
        let Entry::ModelChange(empty) =
            parse_line(r#"{"type":"model_change","id":"x"}"#).expect("parses")
        else {
            panic!("expected a model_change entry");
        };
        assert_eq!(empty.model, None);
    }

    #[test]
    fn effort_change_field() {
        let line = r#"{"type":"effort_change","id":"a10","parentId":"a9","timestamp":"2026-09-30T10:02:01.000Z","effort":"medium"}"#;
        let Entry::EffortChange(ec) = parse_line(line).expect("effort_change parses") else {
            panic!("expected an effort_change entry");
        };
        assert_eq!(ec.envelope.id.as_deref(), Some("a10"));
        assert_eq!(ec.effort.as_deref(), Some("medium"));
    }

    #[test]
    fn session_info_name() {
        let line = r#"{"type":"session_info","id":"a12","parentId":"a11","timestamp":"2026-09-30T10:03:01.000Z","name":"Exporter dry-run flag"}"#;
        let Entry::SessionInfo(si) = parse_line(line).expect("session_info parses") else {
            panic!("expected a session_info entry");
        };
        assert_eq!(si.envelope.id.as_deref(), Some("a12"));
        assert_eq!(si.name.as_deref(), Some("Exporter dry-run flag"));
    }

    #[test]
    fn image_source_block() {
        let line = r#"{"type":"message","id":"a8","parentId":"a7","timestamp":"2026-09-30T10:01:07.000Z","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}],"meta":{"source":"user","messageId":"msg-8"}}}"#;
        let Entry::Message(m) = parse_line(line).expect("image line parses") else {
            panic!("expected a message entry");
        };
        let msg = m.message.as_ref().expect("message");
        let ContentBlock::Image(img) = &msg.content[0] else {
            panic!("expected an image block");
        };
        assert_eq!(
            img.source.get("type").and_then(|v| v.as_str()),
            Some("base64")
        );
        assert_eq!(
            img.source.get("media_type").and_then(|v| v.as_str()),
            Some("image/png")
        );
        assert_eq!(
            img.source.get("data").and_then(|v| v.as_str()),
            Some("aGVsbG8=")
        );
    }

    #[test]
    fn parse_header_rejects_other_types() {
        assert!(parse_header(HEADER).is_some());
        // A message, an unknown type, blank and garbage are not headers.
        assert!(
            parse_header(
                r#"{"type":"message","id":"a1","timestamp":"2026-09-30T10:00:04.000Z","message":{"role":"user","content":[]}}"#
            )
            .is_none()
        );
        assert!(parse_header(r#"{"type":"label","value":"x"}"#).is_none());
        assert!(parse_header("").is_none());
        assert!(parse_header("not json").is_none());
    }

    // S-001
    #[test]
    fn s_001_header_parses_and_version_is_numeric() {
        let real = r#"{"type":"session","version":3,"id":"0b7aa2b4-1111-4111-8111-111111111111","timestamp":"2026-09-30T10:00:00.000Z","cwd":"/project"}"#;
        let Entry::Session(h) = parse_line(real).expect("the real header parses") else {
            panic!("expected a session header");
        };
        assert_eq!(
            h.id.as_deref(),
            Some("0b7aa2b4-1111-4111-8111-111111111111")
        );
        assert_eq!(h.cwd.as_deref(), Some("/project"));
        assert!(h.timestamp.is_some());
        assert_eq!(h.version_number(), Some(3));
        let header = parse_header(real).expect("the real header qualifies");
        assert_eq!(
            header.id.as_deref(),
            Some("0b7aa2b4-1111-4111-8111-111111111111")
        );

        // Absent, string and null versions still parse (the raw value is
        // kept) but state no numeric version, so they never qualify.
        for line in [
            r#"{"type":"session","id":"x"}"#,
            r#"{"type":"session","version":"3","id":"x"}"#,
            r#"{"type":"session","version":null,"id":"x"}"#,
        ] {
            let Entry::Session(h) = parse_line(line).unwrap_or_else(|| panic!("parses: {line}"))
            else {
                panic!("expected a session header: {line}");
            };
            assert_eq!(h.version_number(), None, "not numeric: {line}");
            assert!(parse_header(line).is_none(), "not a header: {line}");
        }
    }

    // S-002
    #[test]
    fn s_002_message_and_blocks_parse_including_image() {
        let assistant = r#"{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-09-30T10:00:09.000Z","model":"claude-sonnet-4-5","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Thread a dry_run flag through the entry point.","signature":"c2lnLTE"},{"type":"text","text":"I will add the flag behind a boolean and extend the test."},{"type":"tool_use","id":"call_01_edit","name":"edit_file","input":{"file_path":"/project/src/exporter.rs","old_string":"pub fn run(config: Config) {","new_string":"pub fn run(config: Config, dry_run: bool) {"}}],"meta":{"source":"model","messageId":"msg-2"}}}"#;
        let Entry::Message(m) = parse_line(assistant).expect("assistant line parses") else {
            panic!("expected a message entry");
        };
        let msg = m.message.as_ref().expect("message");
        assert_eq!(msg.content.len(), 3, "block order preserved");
        assert!(matches!(
            &msg.content[0],
            ContentBlock::Thinking { thinking, signature }
                if thinking == "Thread a dry_run flag through the entry point."
                    && signature.as_deref() == Some("c2lnLTE")
        ));
        assert!(matches!(
            &msg.content[1],
            ContentBlock::Text { text }
                if text == "I will add the flag behind a boolean and extend the test."
        ));
        let ContentBlock::ToolUse(tu) = &msg.content[2] else {
            panic!("third block is a tool_use");
        };
        assert_eq!(tu.id.as_deref(), Some("call_01_edit"));
        assert_eq!(tu.name.as_deref(), Some("edit_file"));
        assert_eq!(
            tu.input.get("file_path").and_then(|v| v.as_str()),
            Some("/project/src/exporter.rs")
        );

        // A tool_result whose content[] carries a nested image block, with no
        // `is_error` at all.
        let result = r#"{"type":"message","id":"a3","parentId":"a2","timestamp":"2026-09-30T10:00:10.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_01_edit","content":[{"type":"text","text":"Edited src/exporter.rs"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}]}],"meta":{"source":"tool","messageId":"msg-3"}}}"#;
        let Entry::Message(m) = parse_line(result).expect("result line parses") else {
            panic!("expected a message entry");
        };
        let ContentBlock::ToolResult(tr) = &m.message.as_ref().expect("message").content[0] else {
            panic!("expected a tool_result block");
        };
        assert_eq!(tr.tool_use_id.as_deref(), Some("call_01_edit"));
        assert_eq!(tr.is_error, None, "absent is_error parses as success");
        assert_eq!(
            tr.content.as_ref(),
            Some(&serde_json::json!([
                {"type": "text", "text": "Edited src/exporter.rs"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
            ])),
            "content[] is kept raw, image block included"
        );

        // A bare image block keeps its source object.
        let image = r#"{"type":"message","id":"a8","timestamp":"2026-09-30T10:01:07.000Z","message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aGVsbG8="}}]}}"#;
        let Entry::Message(m) = parse_line(image).expect("image line parses") else {
            panic!("expected a message entry");
        };
        let ContentBlock::Image(img) = &m.message.as_ref().expect("message").content[0] else {
            panic!("expected an image block");
        };
        assert_eq!(
            img.source,
            serde_json::json!({"type": "base64", "media_type": "image/png", "data": "aGVsbG8="})
        );

        // A message with no `meta` parses with `meta == None`.
        let meta_less = r#"{"type":"message","id":"a9","timestamp":"2026-09-30T10:01:08.000Z","message":{"role":"user","content":[]}}"#;
        let Entry::Message(m) = parse_line(meta_less).expect("meta-less line parses") else {
            panic!("expected a message entry");
        };
        assert!(m.message.as_ref().expect("message").meta.is_none());
    }

    // S-003
    #[test]
    fn s_003_defensive_parse_is_total() {
        // Blank, whitespace-only, non-JSON, truncated and no-`type` lines
        // state nothing at all.
        for line in [
            "",
            "   ",
            "not json",
            r#"{"type":"message""#,
            r#"{"id":"c1","messageId":"m1","turnNumber":1,"prompt":"p"}"#,
        ] {
            assert!(parse_line(line).is_none(), "skippable: {line:?}");
        }
        // Writer-only unknown types are parsed, then skipped.
        for line in [
            r#"{"type":"label","value":"ignored"}"#,
            r#"{"type":"custom_message","text":"x"}"#,
        ] {
            assert!(
                matches!(parse_line(line), Some(Entry::Unknown)),
                "unknown type: {line}"
            );
        }
        // A message with no meta: parsed, not rejected.
        let meta_less = r#"{"type":"message","id":"m1","timestamp":"2026-09-30T10:01:06.000Z","message":{"role":"user","content":[]}}"#;
        let Entry::Message(m) = parse_line(meta_less).expect("parses") else {
            panic!("expected a message entry");
        };
        assert!(m.message.as_ref().expect("message").meta.is_none());
        // A meta with a `messageId` but no `source`: parsed, source None.
        let no_source = r#"{"type":"message","id":"m2","timestamp":"2026-09-30T10:01:06.000Z","message":{"role":"user","content":[{"type":"text","text":"Error: 503 Service Unavailable"}],"meta":{"messageId":"msg-7"}}}"#;
        let Entry::Message(m) = parse_line(no_source).expect("parses") else {
            panic!("expected a message entry");
        };
        assert_eq!(
            m.message
                .as_ref()
                .and_then(|msg| msg.meta.as_ref())
                .and_then(|meta| meta.source.as_deref()),
            None
        );
        // A `tool_result` with no `is_error` parses, and states success.
        let result = r#"{"type":"message","id":"m3","timestamp":"2026-09-30T10:00:10.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_01_edit","content":[{"type":"text","text":"ok"}]}],"meta":{"source":"tool"}}}"#;
        let Entry::Message(m) = parse_line(result).expect("parses") else {
            panic!("expected a message entry");
        };
        let ContentBlock::ToolResult(tr) = &m.message.as_ref().expect("message").content[0] else {
            panic!("expected a tool_result block");
        };
        assert_eq!(tr.is_error, None);
    }
}
