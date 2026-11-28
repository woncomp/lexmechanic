//! Codex rollouts (`~/.codex/{sessions/Y/M/D,archived_sessions}/rollout-<ts>-<thread>.jsonl[.zst]`).
//!
//! The reader uses the `item_completed` events, which carry exactly the user prompts and agent
//! messages. The parallel `response_item` records also hold injected AGENTS.md and developer text.

use super::super::{
    copilot_cli::visible_content,
    jsonl::{Format, Header},
};
use crate::model::*;
use serde_json::{Value, json};

pub(super) struct Codex;

fn item(record: &Value) -> Option<(Role, String, Vec<Value>)> {
    if record.get("type").and_then(Value::as_str)? != "event_msg" {
        return None;
    }
    let payload = record.get("payload")?;
    if payload.get("type").and_then(Value::as_str)? != "item_completed" {
        return None;
    }
    let item = payload.get("item")?;
    let role = match item.get("type").and_then(Value::as_str)? {
        "UserMessage" => Role::User,
        "AgentMessage" => Role::Assistant,
        _ => return None,
    };
    let mut text = Vec::new();
    let mut attachments = Vec::new();
    for part in item.get("content")?.as_array()? {
        match part.get("type").and_then(Value::as_str) {
            Some(kind) if kind.eq_ignore_ascii_case("text") => {
                text.extend(part.get("text").and_then(Value::as_str));
            }
            Some(kind @ ("local_image" | "image")) => {
                attachments.push(json!({ "type": kind, "path": part.get("path") }));
            }
            _ => {}
        }
    }
    Some((role, text.join("\n\n"), attachments))
}

impl Format for Codex {
    fn message(record: &Value, offset: u64, end: u64) -> Option<Message> {
        let (role, raw, attachments) = item(record)?;
        let content = visible_content(&raw);
        if content.trim().is_empty() && attachments.is_empty() {
            return None;
        }
        Some(Message {
            // Agent message ids are long opaque tokens; the per-file ordinal is stable and short.
            id: record.get("ordinal").and_then(Value::as_u64).map_or_else(
                || format!("byte-{offset}"),
                |ordinal| format!("ordinal-{ordinal}"),
            ),
            role,
            time: MessageTime::parse(record.get("timestamp").and_then(Value::as_str)),
            content,
            attachments,
            offset,
            end,
        })
    }

    fn raw(record: &Value) -> Option<String> {
        item(record).map(|(_, raw, _)| raw)
    }

    fn header(record: &Value, header: &mut Header) {
        if record.get("type").and_then(Value::as_str) != Some("session_meta") {
            return;
        }
        let Some(meta) = record.get("payload") else {
            return;
        };
        if header.created.is_none() {
            header.created = meta
                .get("timestamp")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if header.workspace.is_none() {
            header.workspace = meta.get("cwd").and_then(Value::as_str).map(str::to_owned);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_completed_prompts_and_agent_messages_only() {
        let user: Value = serde_json::from_str(
            r#"{"timestamp":"2026-11-20T03:48:05.524Z","ordinal":9,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","id":"u","content":[{"type":"text","text":"hello"},{"type":"local_image","path":"C:\\a.png"}]}}}"#,
        )
        .unwrap();
        let message = Codex::message(&user, 0, 1).unwrap();
        assert_eq!(
            (message.role, message.content.as_str(), message.id.as_str()),
            (Role::User, "hello", "ordinal-9")
        );
        assert_eq!(message.attachments[0]["path"], "C:\\a.png");

        let agent: Value = serde_json::from_str(
            r#"{"timestamp":"2026-11-20T03:48:09.686Z","ordinal":12,"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","id":"x","phase":"commentary","content":[{"type":"Text","text":"on it"}]}}}"#,
        )
        .unwrap();
        assert_eq!(Codex::message(&agent, 0, 1).unwrap().content, "on it");

        for skipped in [
            r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions"}]}}"##,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution"}}}"#,
            r#"{"type":"event_msg","payload":{"type":"token_count"}}"#,
        ] {
            let record: Value = serde_json::from_str(skipped).unwrap();
            assert!(Codex::message(&record, 0, 0).is_none(), "{skipped}");
        }
    }
}
