//! FollowUp handling for Gemini streams and ACP UI actions.
//!
//! FollowUp is not an executable tool. It is an agent-authored next-step
//! action that the ACP client should render as a clickable interaction.

use serde_json::{json, Map, Value};
use agent_client_protocol::schema::v1::{
    Content, ContentBlock, SessionId, SessionNotification, SessionUpdate, TextContent,
    ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolKind,
};
use agent_client_protocol::{Client, ConnectionTo};

const FOLLOW_UP_MARKER: &str = "<FollowUp";

/// ACP extension metadata consumed by FollowUp-aware clients.
///
/// ACP v1 does not define a dedicated action component in `SessionUpdate`,
/// so FollowUp uses the standard `tool_call` envelope with `pending` status
/// plus `_meta.geminiAcp.ui.action` describing a non-executing prompt action.
pub fn action_meta(label: &str, query: &str) -> Map<String, Value> {
    let mut meta = Map::new();
    meta.insert(
        "geminiAcp".into(),
        json!({
            "nonExecutionKind": "action",
            "ui": {
                "component": "action",
                "action": "follow_up",
                "kind": "prompt",
                "label": label,
                "query": query,
                "singleUse": true,
                "dispatch": {
                    "method": "session/prompt",
                    "prompt": query
                }
            }
        }),
    );
    meta
}

/// Emit a real interactive FollowUp action through the ACP session update
/// channel. No permission is requested and no registry tool is executed.
pub fn emit_action(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    label: &str,
    query: &str,
) -> ToolCallId {
    let call_id = ToolCallId::from(format!("followup_{}", uuid::Uuid::new_v4().simple()));
    let title = format!("Follow-up · {}", truncate(label, 80));
    let body = format!("**{}**\n\n{}\n\n_Select cette action pour envoyer cette proposition au modèle._", label, query);
    let meta = action_meta(label, query);
    let content = vec![ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(body))))];
    let tool = ToolCall::new(call_id.clone(), title)
        .kind(ToolKind::Other)
        .status(ToolCallStatus::Pending)
        .content(content)
        .raw_input(json!({ "label": label, "query": query }))
        .meta(meta);

    let _ = cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::ToolCall(tool),
    ));
    call_id
}

#[derive(Debug, Default)]
pub struct StreamNormalizer {
    pending: String,
}

impl StreamNormalizer {
    pub fn push(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        self.drain(false)
    }

    pub fn finish(&mut self) -> String { self.drain(true) }

    fn drain(&mut self, final_flush: bool) -> String {
        let mut out = String::new();
        loop {
            let Some(start) = self.pending.find(FOLLOW_UP_MARKER) else {
                if final_flush {
                    out.push_str(&self.pending);
                    self.pending.clear();
                    return out;
                }
                let keep = partial_marker_len(&self.pending);
                let emit_len = self.pending.len().saturating_sub(keep);
                if emit_len > 0 {
                    out.push_str(&self.pending[..emit_len]);
                    self.pending = self.pending[emit_len..].to_owned();
                }
                return out;
            };

            if start > 0 {
                out.push_str(&self.pending[..start]);
                self.pending = self.pending[start..].to_owned();
            }

            let Some(end) = find_tag_end(&self.pending[FOLLOW_UP_MARKER.len()..]) else {
                if final_flush {
                    self.pending.clear();
                }
                return out;
            };

            let consume = FOLLOW_UP_MARKER.len() + end + 1;
            self.pending = self.pending[consume..].to_owned();
        }
    }
}

/// FollowUp is parsed by the runtime parser. This compatibility helper keeps
/// the old turn orchestration API while avoiding duplicate transformations.
pub fn replace_components(input: &str) -> String { input.to_owned() }

fn partial_marker_len(input: &str) -> usize {
    let marker = FOLLOW_UP_MARKER.as_bytes();
    let bytes = input.as_bytes();
    let max = bytes.len().min(marker.len().saturating_sub(1));
    for len in (1..=max).rev() {
        if bytes[bytes.len() - len..] == marker[..len] {
            return len;
        }
    }
    0
}

fn find_tag_end(input: &str) -> Option<usize> {
    let mut quote = None;
    for (index, byte) in input.as_bytes().iter().copied().enumerate() {
        match quote {
            Some(current) if byte == current => quote = None,
            Some(_) => {}
            None if byte == b'\'' || byte == b'"' => quote = Some(byte),
            None if byte == b'>' => return Some(index),
            None => {}
        }
    }
    None
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max { return value.to_owned(); }
    format!("{}…", value.chars().take(max).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_meta_is_non_executing_and_clickable() {
        let meta = action_meta("Run tests", "cargo test");
        assert_eq!(meta["geminiAcp"]["nonExecutionKind"], "action");
        assert_eq!(meta["geminiAcp"]["ui"]["component"], "action");
        assert_eq!(meta["geminiAcp"]["ui"]["action"], "follow_up");
        assert_eq!(meta["geminiAcp"]["ui"]["query"], "cargo test");
        assert_eq!(meta["geminiAcp"]["ui"]["singleUse"], true);
        assert_eq!(meta["geminiAcp"]["ui"]["dispatch"]["method"], "session/prompt");
        assert_eq!(meta["geminiAcp"]["ui"]["dispatch"]["prompt"], "cargo test");
    }

    #[test]
    fn removes_complete_follow_up_from_stream() {
        let mut normalizer = StreamNormalizer::default();
        assert_eq!(normalizer.push("hello <FollowUp label=\"Run tests\" query=\"cargo test\" />"), "hello ");
        assert_eq!(normalizer.finish(), "");
    }

    #[test]
    fn handles_split_marker_at_every_prefix_length() {
        for split in 1.."<FollowUp".len() {
            let mut normalizer = StreamNormalizer::default();
            let (left, right) = "<FollowUp label=\"Run\" query=\"cargo test\" />".split_at(split);
            assert_eq!(normalizer.push(&format!("before{left}")), "before");
            assert_eq!(normalizer.push(right), "");
            assert_eq!(normalizer.finish(), "");
        }
    }

    #[test]
    fn does_not_stop_at_gt_inside_quotes() {
        let mut normalizer = StreamNormalizer::default();
        assert_eq!(normalizer.push("hello <FollowUp label=\"A > B\" query=\"cargo test\" /> world"), "hello  world");
    }

    #[test]
    fn incomplete_follow_up_is_discarded_on_finish() {
        let mut normalizer = StreamNormalizer::default();
        assert_eq!(normalizer.push("hello <FollowUp label=\"Run"), "hello ");
        assert_eq!(normalizer.finish(), "");
    }
}
