//! Claude Code SDK transcripts (`~/.claude/projects/<slug>/<sdk-session>.jsonl`).
//!
//! Only real prompts and assistant text are messages. Tool results, thinking, tool calls and the
//! `attachment` records that carry injected reminders are bookkeeping and stay out of the reader.

use super::super::{
    copilot_cli::visible_content,
    jsonl::{Format, Header},
};
use crate::model::*;
use serde_json::{Value, json};

pub(super) struct Claude;

fn text_blocks(record: &Value) -> Option<(Role, String, Vec<Value>)> {
    if record.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let role = match record.get("type").and_then(Value::as_str)? {
        "user" => {
            let is_meta = record.get("isMeta").and_then(Value::as_bool) == Some(true);
            let system = record.get("promptSource").and_then(Value::as_str) == Some("system");
            if is_meta || system {
                return None;
            }
            Role::User
        }
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let content = record.pointer("/message/content")?;
    let mut text = Vec::new();
    let mut attachments = Vec::new();
    match content {
        Value::String(value) => text.push(value.as_str()),
        Value::Array(blocks) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => text.extend(block.get("text").and_then(Value::as_str)),
                    // Image bytes stay in the source file; only their presence is recorded.
                    Some("image") => attachments.push(json!({
                        "type": "image",
                        "media_type": block.pointer("/source/media_type"),
                    })),
                    _ => {}
                }
            }
        }
        _ => return None,
    }
    Some((role, text.join("\n\n"), attachments))
}

impl Format for Claude {
    fn message(record: &Value, offset: u64, end: u64) -> Option<Message> {
        let (role, raw, attachments) = text_blocks(record)?;
        let content = visible_content(&raw);
        if content.trim().is_empty() && attachments.is_empty() {
            return None;
        }
        Some(Message {
            id: record
                .get("uuid")
                .and_then(Value::as_str)
                .map_or_else(|| format!("byte-{offset}"), str::to_owned),
            role,
            time: MessageTime::parse(record.get("timestamp").and_then(Value::as_str)),
            content,
            attachments,
            offset,
            end,
        })
    }

    fn raw(record: &Value) -> Option<String> {
        text_blocks(record).map(|(_, raw, _)| raw)
    }

    fn header(record: &Value, header: &mut Header) {
        if header.created.is_none() {
            header.created = record
                .get("timestamp")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if header.workspace.is_none() {
            header.workspace = record.get("cwd").and_then(Value::as_str).map(str::to_owned);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible(line: &str) -> Option<(Role, String)> {
        let record: Value = serde_json::from_str(line).unwrap();
        Claude::message(&record, 0, 0).map(|message| (message.role, message.content))
    }

    #[test]
    fn keeps_prompts_and_replies_but_not_tool_traffic() {
        assert_eq!(
            visible(
                r#"{"type":"user","uuid":"a","message":{"content":[{"type":"text","text":"hi"}]}}"#
            ),
            Some((Role::User, "hi".into()))
        );
        assert_eq!(
            visible(
                r#"{"type":"assistant","uuid":"b","message":{"content":[{"type":"text","text":"yo"}]}}"#
            ),
            Some((Role::Assistant, "yo".into()))
        );
        for skipped in [
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"user","promptSource":"system","message":{"content":"<task-notification>"}}"#,
            r#"{"type":"user","isSidechain":true,"message":{"content":"sub"}}"#,
            r#"{"type":"attachment","rendered":"<system-reminder>x</system-reminder>"}"#,
        ] {
            assert_eq!(visible(skipped), None, "{skipped}");
        }
    }

    #[test]
    fn images_are_metadata_only() {
        let record: Value = serde_json::from_str(
            r#"{"type":"user","uuid":"a","message":{"content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}]}}"#,
        )
        .unwrap();
        let message = Claude::message(&record, 0, 0).unwrap();
        assert!(message.content.is_empty());
        assert_eq!(message.attachments[0]["media_type"], "image/png");
        assert!(!message.attachments[0].to_string().contains("AAAA"));
    }
}
