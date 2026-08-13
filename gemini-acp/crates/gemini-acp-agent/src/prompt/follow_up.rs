//! Gemini `<FollowUp>` component support.
//!
//! Gemini can emit one explicit next-step action as:
//! `<FollowUp label="…" query="…" />`.
//!
//! ACP v1 does not define a dedicated FollowUp content block, so the adapter
//! normalizes the component into Markdown link syntax while preserving the
//! action semantics in the rendered text: `[label](query)`.

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowUp {
    pub label: String,
    pub query: String,
}

impl FollowUp {
    pub fn new(label: impl Into<String>, query: impl Into<String>) -> Self {
        Self { label: label.into(), query: query.into() }
    }

    pub fn render_markdown(&self) -> String {
        format!("[{}]({})", escape_markdown_label(&self.label), self.query)
    }
}

fn tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?s)<FollowUp\s+label\s*=\s*(?:\"([^\"]*)\"|'([^']*)')\s+query\s*=\s*(?:\"([^\"]*)\"|'([^']*)')\s*/\s*>"#,
        )
        .expect("FollowUp regex must be valid")
    })
}

pub fn parse(input: &str) -> Vec<FollowUp> {
    tag_re()
        .captures_iter(input)
        .filter_map(|caps| {
            let label = caps.get(1).or_else(|| caps.get(2))?.as_str().trim();
            let query = caps.get(3).or_else(|| caps.get(4))?.as_str().trim();
            if label.is_empty() || query.is_empty() {
                None
            } else {
                Some(FollowUp::new(label, query))
            }
        })
        .take(1)
        .collect()
}

pub fn replace_components(input: &str) -> String {
    tag_re()
        .replace_all(input, |caps: &regex::Captures<'_>| {
            let label = caps.get(1).or_else(|| caps.get(2)).map(|v| v.as_str()).unwrap_or("").trim();
            let query = caps.get(3).or_else(|| caps.get(4)).map(|v| v.as_str()).unwrap_or("").trim();
            if label.is_empty() || query.is_empty() {
                caps.get(0).map(|v| v.as_str()).unwrap_or("").to_owned()
            } else {
                FollowUp::new(label, query).render_markdown()
            }
        })
        .into_owned()
}

/// Streaming normalizer: keep a possible partial `<FollowUp ...` tag buffered
/// until it is either complete or proven to be ordinary text.
#[derive(Debug, Default)]
pub struct StreamNormalizer {
    pending: String,
}

impl StreamNormalizer {
    pub fn push(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        self.drain(false)
    }

    pub fn finish(&mut self) -> String {
        self.drain(true)
    }

    fn drain(&mut self, final_flush: bool) -> String {
        let mut out = String::new();
        loop {
            let Some(start) = self.pending.find("<FollowUp") else {
                if final_flush {
                    out.push_str(&self.pending);
                    self.pending.clear();
                    return out;
                }
                let keep = self
                    .pending
                    .char_indices()
                    .rev()
                    .take_while(|(_, c)| *c != '<')
                    .map(|(i, _)| i)
                    .last()
                    .unwrap_or(self.pending.len());
                let emit_len = keep.min(self.pending.len());
                if emit_len == self.pending.len() {
                    out.push_str(&self.pending);
                    self.pending.clear();
                } else if emit_len > 0 {
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
            let end = end + 2;
            let candidate = self.pending[..end].to_owned();
            let rendered = replace_components(&candidate);
            if rendered == candidate {
                out.push_str(&candidate);
            } else {
                out.push_str("\n\n");
                out.push_str(&rendered);
            }
            self.pending = self.pending[end..].to_owned();
        }
    }
}

fn escape_markdown_label(label: &str) -> String {
    label.replace('[', "\\[").replace(']', "\\]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_double_quoted_follow_up() {
        assert_eq!(
            parse(r#"Before <FollowUp label="Run tests" query="run cargo test" /> after"#),
            vec![FollowUp::new("Run tests", "run cargo test")]
        );
    }

    #[test]
    fn parses_single_quoted_follow_up() {
        assert_eq!(
            parse("<FollowUp label='Inspect errors' query='show the failing checks' />"),
            vec![FollowUp::new("Inspect errors", "show the failing checks")]
        );
    }

    #[test]
    fn only_first_follow_up_is_exposed() {
        let value = "<FollowUp label=\"One\" query=\"1\" /> <FollowUp label=\"Two\" query=\"2\" />";
        assert_eq!(parse(value), vec![FollowUp::new("One", "1")]);
    }

    #[test]
    fn invalid_empty_attributes_are_ignored() {
        assert!(parse(r#"<FollowUp label="" query="x" />"#).is_empty());
        assert!(parse(r#"<FollowUp label="x" query="" />"#).is_empty());
    }

    #[test]
    fn replaces_with_markdown_link() {
        assert_eq!(
            replace_components(r#"Next step: <FollowUp label="Run tests" query="run cargo test" />"#),
            "Next step: [Run tests](run cargo test)"
        );
    }

    #[test]
    fn streaming_filter_handles_split_tag() {
        let mut normalizer = StreamNormalizer::default();
        assert_eq!(normalizer.push("hello <FollowUp label=\"Run"), "hello ");
        assert_eq!(normalizer.push(" tests\" query=\"cargo test\" />"), "\n\n[Run tests](cargo test)");
        assert_eq!(normalizer.finish(), "");
    }
}
