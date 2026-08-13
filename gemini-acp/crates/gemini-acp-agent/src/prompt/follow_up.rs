//! FollowUp handling for Gemini streams and ACP interactive elicitation.
//!
//! FollowUp is an agent-authored next-step action. ACP v1 does not expose a
//! generic inline button component, so the action is presented through the
//! standard client-side elicitation primitive, which existing ACP hosts know
//! how to render and resolve interactively.

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, ElicitationAction, ElicitationContentValue,
    ElicitationFormMode, ElicitationPropertySchema, ElicitationSchema,
    ElicitationSessionScope, SessionId,
};
use agent_client_protocol::{Client, ConnectionTo};
use serde_json::{json, Value};

const FOLLOW_UP_MARKER: &str = "<FollowUp";
const SKIP_VALUE: &str = "__followup_skip__";
const SELECT_VALUE: &str = "__followup_select__";

/// Wait for the user to select or dismiss a FollowUp suggestion.
///
/// Returns `Some(query)` when the user explicitly selects the action and
/// `None` when the suggestion is dismissed/cancelled. The selected query can
/// then be fed back into the current model turn without inventing a fake
/// ToolCall execution.
pub async fn request_action(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    label: &str,
    query: &str,
) -> Result<Option<String>, String> {
    let mut schema = ElicitationSchema::new();

    let action_schema: ElicitationPropertySchema = serde_json::from_value(json!({
        "type": "string",
        "title": "Action",
        "description": query,
        "oneOf": [
            {
                "const": SELECT_VALUE,
                "title": label,
                "description": query
            },
            {
                "const": SKIP_VALUE,
                "title": "Ignorer",
                "description": "Ne pas exécuter cette proposition."
            }
        ],
        "_meta": {
            "geminiAcp": {
                "component": "follow_up_action",
                "label": label,
                "query": query,
                "submitBehavior": "prompt"
            }
        }
    }))
    .map_err(|error| format!("invalid FollowUp elicitation schema: {error}"))?;

    schema = schema.property("action", action_schema, true);

    let mode = ElicitationFormMode::new(ElicitationSessionScope::new(session_id.clone()), schema);
    let request = CreateElicitationRequest::new(
        mode,
        format!("{label}\n\n{query}"),
    );

    let response = cx
        .send_request(request)
        .block_task()
        .await
        .map_err(|error| format!("ACP FollowUp elicitation failed: {error}"))?;

    match response.action {
        ElicitationAction::Accept(accept) => {
            let selected = accept
                .content
                .and_then(|content| content.get("action").cloned())
                .and_then(|value| match value {
                    ElicitationContentValue::String(value) => Some(value),
                    _ => None,
                });

            match selected.as_deref() {
                Some(SELECT_VALUE) => Ok(Some(query.to_owned())),
                Some(SKIP_VALUE) | None => Ok(None),
                Some(_) => Err("ACP returned an unknown FollowUp action value".into()),
            }
        }
        ElicitationAction::Decline | ElicitationAction::Cancel => Ok(None),
        _ => Err("ACP returned an unsupported FollowUp elicitation action".into()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_followup_values_stable() {
        assert_eq!(SELECT_VALUE, "__followup_select__");
        assert_eq!(SKIP_VALUE, "__followup_skip__");
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
