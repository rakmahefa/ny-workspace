//! Streaming filter for Gemini `<FollowUp>` components.
//!
//! FollowUp is represented as a real builtin `ToolCall` by the runtime parser.
//! This module only prevents the XML component from leaking into streamed
//! `agent_message_chunk` output before the final tool-call parsing phase.

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
            let Some(start) = self.pending.find("<FollowUp") else {
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

            let Some(end) = self.pending.find("/>") else {
                if final_flush {
                    out.push_str(&self.pending);
                    self.pending.clear();
                }
                return out;
            };

            // The raw assistant buffer remains untouched and is parsed later by
            // `parse_tool_calls` into the actual FollowUp builtin ToolCall.
            self.pending = self.pending[end + 2..].to_owned();
        }
    }
}

/// Kept as a source-compatible helper for the existing turn orchestrator.
/// FollowUp is no longer rendered here; the runtime parser owns that concern.
pub fn replace_components(input: &str) -> String { input.to_owned() }

fn partial_marker_len(input: &str) -> usize {
    let marker = b"<FollowUp";
    let bytes = input.as_bytes();
    let max = bytes.len().min(marker.len());
    for len in (1..=max).rev() {
        if bytes[bytes.len() - len..] == marker[..len] {
            return len;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_complete_follow_up_from_stream() {
        let mut normalizer = StreamNormalizer::default();
        assert_eq!(normalizer.push("hello <FollowUp label=\"Run tests\" query=\"cargo test\" />"), "hello ");
        assert_eq!(normalizer.finish(), "");
    }

    #[test]
    fn handles_split_follow_up() {
        let mut normalizer = StreamNormalizer::default();
        assert_eq!(normalizer.push("hello <FollowUp label=\"Run"), "hello ");
        assert_eq!(normalizer.push(" tests\" query=\"cargo test\" />"), "");
        assert_eq!(normalizer.finish(), "");
    }

    #[test]
    fn compatibility_replace_is_noop() {
        let input = "text <FollowUp label=\"Run\" query=\"cargo test\" />";
        assert_eq!(replace_components(input), input);
    }
}
