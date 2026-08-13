//! Streaming filter for Gemini `<FollowUp>` components.
//!
//! FollowUp is represented as a real builtin `ToolCall` by the runtime parser.
//! This module only prevents the XML component from leaking into streamed
//! `agent_message_chunk` output before the final tool-call parsing phase.
//!
//! The model stream is chunked arbitrarily, so this parser must never assume
//! that `<FollowUp ... />` arrives in one piece. It therefore keeps an
//! incomplete marker/component in `pending` until the full tag is available.

const FOLLOW_UP_MARKER: &str = "<FollowUp";

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
                    // The raw assistant buffer is parsed independently after
                    // streaming. Never leak an incomplete FollowUp fragment.
                    self.pending.clear();
                }
                return out;
            };

            // `end` is relative to the substring after `<FollowUp`.
            let consume = FOLLOW_UP_MARKER.len() + end + 1;
            self.pending = self.pending[consume..].to_owned();
        }
    }
}

/// Kept as a source-compatible helper for the existing turn orchestrator.
/// FollowUp is no longer rendered here; the runtime parser owns that concern.
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

/// Find the first `>` that terminates a FollowUp tag, ignoring `>` inside
/// quoted attribute values. The returned index is relative to `input`.
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
    fn removes_complete_follow_up_from_stream() {
        let mut normalizer = StreamNormalizer::default();
        assert_eq!(normalizer.push("hello <FollowUp label=\"Run tests\" query=\"cargo test\" />"), "hello ");
        assert_eq!(normalizer.finish(), "");
    }

    #[test]
    fn handles_split_follow_up() {
        let mut normalizer = StreamNormalizer::default();
        assert_eq!(normalizer.push("hello <Follow"), "hello ");
        assert_eq!(normalizer.push("Up label=\"Run"), "");
        assert_eq!(normalizer.push(" tests\" query=\"cargo test\" />"), "");
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
    fn does_not_drop_unrelated_gt_inside_quotes() {
        let mut normalizer = StreamNormalizer::default();
        assert_eq!(
            normalizer.push("hello <FollowUp label=\"A > B\" query=\"cargo test\" /> world"),
            "hello  world"
        );
    }

    #[test]
    fn incomplete_follow_up_is_discarded_on_finish() {
        let mut normalizer = StreamNormalizer::default();
        assert_eq!(normalizer.push("hello <FollowUp label=\"Run"), "hello ");
        assert_eq!(normalizer.finish(), "");
    }

    #[test]
    fn compatibility_replace_is_noop() {
        let input = "text <FollowUp label=\"Run\" query=\"cargo test\" />";
        assert_eq!(replace_components(input), input);
    }
}
