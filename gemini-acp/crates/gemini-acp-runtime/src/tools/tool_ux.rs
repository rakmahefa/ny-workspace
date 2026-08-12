//! Claude-style ACP tool UX mapping for Gemini tools.
//!
//! The module keeps tool-call presentation and tool-result presentation separate.
//! The UI target is deliberately quiet and high-signal:
//! - compact, human-readable titles;
//! - project-relative paths for display;
//! - ACP locations with source lines for follow-along;
//! - diffs for writes/edits;
//! - terminal blocks for shell execution;
//! - normalized search results instead of leaking absolute workspace paths.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    Content, ContentBlock, Diff, TextContent, ToolCallContent, ToolCallLocation, ToolCallStatus,
};

use super::sandbox::{RiskLevel, ShellAnalysis, ShellSandbox};

const MAX_DIFF_OLD_TEXT_BYTES: u64 = 64 * 1024;
const MAX_RAW_INPUT_CHARS: usize = 8 * 1024;
const MAX_RESULT_LOCATIONS: usize = 8;

#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub title: String,
    pub kind: agent_client_protocol::schema::v1::ToolKind,
    pub content: Vec<ToolCallContent>,
    pub locations: Vec<ToolCallLocation>,
}

impl ToolInfo {
    pub fn build(
        name: &str,
        args: &serde_json::Value,
        cwd: &Path,
        terminal_id: Option<&str>,
    ) -> Self {
        match name {
            "file_read" => file_read(args, cwd),
            "file_write" => file_write(args, cwd),
            "file_edit" | "replace_in_file" => file_edit(args, cwd),
            "search" => search(args, cwd),
            "search_and_read" => search_and_read(args, cwd),
            "shell_exec" => shell_exec(args, terminal_id),
            _ => generic(name, args),
        }
    }
}

pub fn bounded_raw_input(args: &serde_json::Value) -> serde_json::Value {
    let mut value = args.clone();
    let Some(object) = value.as_object_mut() else { return value };
    let Some(content_value) = object.get_mut("content") else { return value };
    let Some(content) = content_value.as_str() else { return value };

    let count = content.chars().count();
    if count <= MAX_RAW_INPUT_CHARS {
        return value;
    }

    let preview: String = content.chars().take(MAX_RAW_INPUT_CHARS).collect();
    *content_value = serde_json::Value::String(format!(
        "{preview}\n… [{} chars omitted from ACP display]",
        count - MAX_RAW_INPUT_CHARS
    ));
    value
}

#[derive(Debug, Clone)]
pub struct ResultUpdate {
    pub status: ToolCallStatus,
    pub content: Vec<ToolCallContent>,
    pub locations: Vec<ToolCallLocation>,
}

pub fn result_update(
    tool_name: &str,
    args: &serde_json::Value,
    result: &str,
    is_ok: bool,
    cwd: &Path,
    terminal_id: Option<&str>,
) -> ResultUpdate {
    match tool_name {
        "file_read" => file_read_result(args, result, is_ok, cwd),
        "shell_exec" => shell_result(result, is_ok, terminal_id),
        "file_write" | "file_edit" | "replace_in_file" => ResultUpdate {
            status: if is_ok { ToolCallStatus::Completed } else { ToolCallStatus::Failed },
            // Successful edits are already represented by the initial ACP Diff.
            // Repeating the whole file here makes the UI noisy.
            content: if is_ok { vec![] } else { text_content(result, true) },
            locations: file_location(args, cwd),
        },
        "search" | "search_and_read" => {
            let locations = search_result_locations(result, cwd);
            ResultUpdate {
                status: if is_ok { ToolCallStatus::Completed } else { ToolCallStatus::Failed },
                content: if is_ok {
                    text_content(&normalize_search_result(tool_name, result, cwd), false)
                } else {
                    text_content(result, true)
                },
                locations: if locations.is_empty() {
                    search_location(args, cwd)
                } else {
                    locations
                },
            }
        }
        _ => ResultUpdate {
            status: if is_ok { ToolCallStatus::Completed } else { ToolCallStatus::Failed },
            content: text_content(result, !is_ok),
            locations: vec![],
        },
    }
}

fn file_read(args: &serde_json::Value, cwd: &Path) -> ToolInfo {
    let path = arg_str(args, "path").unwrap_or("File");
    let display = display_path(path, cwd);
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1).max(1);
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(500).max(1);
    ToolInfo {
        title: format!("Read {display} ({}-{})", offset, offset + limit - 1),
        kind: agent_client_protocol::schema::v1::ToolKind::Read,
        content: vec![],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd)).line(offset as u32)],
    }
}

fn file_read_result(
    args: &serde_json::Value,
    result: &str,
    is_ok: bool,
    cwd: &Path,
) -> ResultUpdate {
    if !is_ok {
        return ResultUpdate {
            status: ToolCallStatus::Failed,
            content: text_content(result, true),
            locations: file_location(args, cwd),
        };
    }

    let start = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
    let numbered = result
        .trim_end_matches('\n')
        .split('\n')
        .enumerate()
        .map(|(idx, line)| format!("{}\t{}", start + idx, line))
        .collect::<Vec<_>>()
        .join("\n");

    ResultUpdate {
        status: ToolCallStatus::Completed,
        content: if numbered.is_empty() {
            vec![]
        } else {
            text_content(&numbered, false)
        },
        locations: file_location(args, cwd),
    }
}

fn file_write(args: &serde_json::Value, cwd: &Path) -> ToolInfo {
    let path = arg_str(args, "path").unwrap_or("File");
    let content = arg_str(args, "content").unwrap_or("");
    let resolved = resolve_path(path, cwd);
    let diff = Diff::new(resolved.clone(), content.to_owned()).old_text(read_old_text(&resolved));

    ToolInfo {
        title: format!("Write {}", display_path(path, cwd)),
        kind: agent_client_protocol::schema::v1::ToolKind::Edit,
        content: vec![ToolCallContent::Diff(diff)],
        locations: vec![ToolCallLocation::new(resolved)],
    }
}

fn file_edit(args: &serde_json::Value, cwd: &Path) -> ToolInfo {
    let path = arg_str(args, "path").unwrap_or("File");
    let old = arg_str(args, "old_string").unwrap_or("");
    let new = arg_str(args, "new_string").unwrap_or("");
    let resolved = resolve_path(path, cwd);
    let old_text = if old.is_empty() {
        read_old_text(&resolved)
    } else {
        Some(old.to_owned())
    };
    let diff = Diff::new(resolved.clone(), new.to_owned()).old_text(old_text);

    ToolInfo {
        title: format!("Edit {}", display_path(path, cwd)),
        kind: agent_client_protocol::schema::v1::ToolKind::Edit,
        content: vec![ToolCallContent::Diff(diff)],
        locations: vec![ToolCallLocation::new(resolved)],
    }
}

fn search(args: &serde_json::Value, cwd: &Path) -> ToolInfo {
    let pattern = arg_str(args, "pattern").unwrap_or("");
    let path = arg_str(args, "path").unwrap_or(".");

    ToolInfo {
        title: if path == "." {
            format!("Find `{}`", truncate(pattern, 72))
        } else {
            format!("Find `{}` in {}", truncate(pattern, 56), display_path(path, cwd))
        },
        kind: agent_client_protocol::schema::v1::ToolKind::Search,
        content: vec![],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd))],
    }
}

fn search_and_read(args: &serde_json::Value, cwd: &Path) -> ToolInfo {
    let pattern = arg_str(args, "pattern").unwrap_or("");
    let path = arg_str(args, "path").unwrap_or(".");

    ToolInfo {
        title: if path == "." {
            format!("Find excerpts for `{}`", truncate(pattern, 56))
        } else {
            format!(
                "Find excerpts for `{}` in {}",
                truncate(pattern, 40),
                display_path(path, cwd)
            )
        },
        kind: agent_client_protocol::schema::v1::ToolKind::Search,
        content: vec![],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd))],
    }
}

fn shell_exec(args: &serde_json::Value, terminal_id: Option<&str>) -> ToolInfo {
    let command = arg_str(args, "command").unwrap_or("");
    let content = terminal_id
        .map(|id| {
            vec![ToolCallContent::Terminal(
                agent_client_protocol::schema::v1::Terminal::new(id.to_owned()),
            )]
        })
        .unwrap_or_default();

    ToolInfo {
        // The command itself is the most useful shell title. Avoid adding a
        // generic "Exec:" prefix so clients can display it as a terminal card.
        title: if command.is_empty() {
            "Terminal".into()
        } else {
            truncate(command, 96)
        },
        kind: agent_client_protocol::schema::v1::ToolKind::Execute,
        content,
        locations: vec![],
    }
}

fn generic(name: &str, args: &serde_json::Value) -> ToolInfo {
    ToolInfo {
        title: name.to_owned(),
        kind: agent_client_protocol::schema::v1::ToolKind::Other,
        content: if args.as_object().map_or(true, |obj| obj.is_empty()) {
            vec![]
        } else {
            vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
                TextContent::new(concise_args(args)),
            )))]
        },
        locations: vec![],
    }
}

fn shell_result(result: &str, is_ok: bool, terminal_id: Option<&str>) -> ResultUpdate {
    let content = terminal_id
        .map(|id| {
            vec![ToolCallContent::Terminal(
                agent_client_protocol::schema::v1::Terminal::new(id.to_owned()),
            )]
        })
        .unwrap_or_else(|| {
            text_content(&format!("```console\n{}\n```", result.trim_end()), !is_ok)
        });

    ResultUpdate {
        status: if is_ok { ToolCallStatus::Completed } else { ToolCallStatus::Failed },
        content,
        locations: vec![],
    }
}

fn text_content(text: &str, error: bool) -> Vec<ToolCallContent> {
    let rendered = if error {
        format!("```text\n{text}\n```")
    } else {
        text.to_owned()
    };

    vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
        TextContent::new(rendered),
    )))]
}

fn file_location(args: &serde_json::Value, cwd: &Path) -> Vec<ToolCallLocation> {
    arg_str(args, "path")
        .map(|path| vec![ToolCallLocation::new(resolve_path(path, cwd))])
        .unwrap_or_default()
}

fn search_location(args: &serde_json::Value, cwd: &Path) -> Vec<ToolCallLocation> {
    let path = arg_str(args, "path").unwrap_or(".");
    vec![ToolCallLocation::new(resolve_path(path, cwd))]
}

fn normalize_search_result(tool_name: &str, result: &str, cwd: &Path) -> String {
    let mut output = String::with_capacity(result.len());

    for (index, line) in result.lines().enumerate() {
        if index > 0 {
            output.push('\n');
        }

        let normalized = if tool_name == "search_and_read" && line.starts_with("## ") {
            normalize_heading_path(line, cwd)
        } else {
            normalize_match_line(line, cwd)
        };
        output.push_str(&normalized);
    }

    if result.ends_with('\n') {
        output.push('\n');
    }

    output
}

fn normalize_heading_path(line: &str, cwd: &Path) -> String {
    let body = &line[3..];
    let Some((path, suffix)) = split_path_line(body) else {
        return line.to_owned();
    };
    format!("## {}{}", display_path(path, cwd), suffix)
}

fn normalize_match_line(line: &str, cwd: &Path) -> String {
    let Some((path, suffix)) = split_path_line(line) else {
        return line.to_owned();
    };

    if path.starts_with('(') || path.starts_with('…') {
        return line.to_owned();
    }

    format!("{}{}", display_path(path, cwd), suffix)
}

/// Splits `path:line:rest` and preserves the `:line:rest` suffix.
fn split_path_line(line: &str) -> Option<(&str, &str)> {
    let marker = line.find(':')?;
    let path = &line[..marker];
    if path.is_empty() {
        return None;
    }

    let rest = &line[marker..];
    let mut suffix_body = rest.strip_prefix(':')?;
    let line_number = suffix_body.split(':').next()?.parse::<usize>().ok()?;

    // Preserve the leading colon before the line number and every character
    // after it, e.g. `:2:Ligne...`.
    let digits = line_number.to_string();
    let suffix_start = 1 + digits.len();
    suffix_body = &rest[suffix_start..];
    Some((path, suffix_body))
}

fn search_result_locations(result: &str, cwd: &Path) -> Vec<ToolCallLocation> {
    let mut locations = Vec::new();
    let mut seen = BTreeSet::new();

    for line in result.lines() {
        let Some((path, suffix)) = split_path_line(line) else { continue };
        let Some(line_number) = suffix
            .strip_prefix(':')
            .and_then(|value| value.split(':').next())
            .and_then(|value| value.parse::<u32>().ok())
        else {
            continue;
        };

        let resolved = resolve_path(path, cwd);
        let key = format!("{}:{line_number}", resolved.display());

        if !seen.insert(key) {
            continue;
        }

        locations.push(ToolCallLocation::new(resolved).line(line_number));
        if locations.len() >= MAX_RESULT_LOCATIONS {
            break;
        }
    }

    // search_and_read headings carry the same `path:line` information but are
    // preceded by `## `, so parse those as a fallback.
    if locations.is_empty() {
        for line in result.lines().filter(|line| line.starts_with("## ")) {
            let body = &line[3..];
            let Some((path, suffix)) = split_path_line(body) else { continue };
            let Some(line_number) = suffix
                .strip_prefix(':')
                .and_then(|value| value.split(':').next())
                .and_then(|value| value.parse::<u32>().ok())
            else {
                continue;
            };

            let resolved = resolve_path(path, cwd);
            let key = format!("{}:{line_number}", resolved.display());
            if seen.insert(key) {
                locations.push(ToolCallLocation::new(resolved).line(line_number));
            }

            if locations.len() >= MAX_RESULT_LOCATIONS {
                break;
            }
        }
    }

    locations
}

fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(serde_json::Value::as_str)
}

fn resolve_path(path: &str, cwd: &Path) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
    }
}

fn display_path(path: &str, cwd: &Path) -> String {
    let resolved = resolve_path(path, cwd);
    match resolved.strip_prefix(cwd) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.display().to_string(),
        Ok(_) => ".".into(),
        Err(_) => path.to_owned(),
    }
}

fn read_old_text(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_DIFF_OLD_TEXT_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    format!("{}…", value.chars().take(max).collect::<String>())
}

fn concise_args(args: &serde_json::Value) -> String {
    let Some(obj) = args.as_object() else { return "{}".into() };
    format!(
        "Arguments: {}",
        obj.keys().cloned().collect::<Vec<_>>().join(", ")
    )
}

pub fn classify_risk(name: &str, args: &serde_json::Value) -> RiskLevel {
    match name {
        "shell_exec" => arg_str(args, "command")
            .and_then(|command| ShellSandbox::new().analyze_command(command).ok())
            .map(|ShellAnalysis { risk, .. }| risk)
            .unwrap_or(RiskLevel::Critical),
        "file_write" | "file_edit" | "replace_in_file" => RiskLevel::Medium,
        _ => RiskLevel::Low,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_titles_match_markdown_ux() {
        let cwd = Path::new("/tmp/test-workspace");

        assert_eq!(
            ToolInfo::build(
                "file_read",
                &serde_json::json!({"path":"test_tool_demo.txt","offset":1,"limit":10}),
                cwd,
                None,
            )
            .title,
            "Read test_tool_demo.txt (1-10)"
        );

        assert_eq!(
            ToolInfo::build(
                "file_write",
                &serde_json::json!({"path":"test_tool_demo.txt","content":"hello"}),
                cwd,
                None,
            )
            .title,
            "Write test_tool_demo.txt"
        );

        assert_eq!(
            ToolInfo::build(
                "file_edit",
                &serde_json::json!({"path":"test_tool_demo.txt","old_string":"a","new_string":"b"}),
                cwd,
                None,
            )
            .title,
            "Edit test_tool_demo.txt"
        );

        assert_eq!(
            ToolInfo::build(
                "search",
                &serde_json::json!({"path":"test_tool_demo.txt","pattern":"réussi"}),
                cwd,
                None,
            )
            .title,
            "Find `réussi` in test_tool_demo.txt"
        );

        assert_eq!(
            ToolInfo::build(
                "search_and_read",
                &serde_json::json!({"path":"test_tool_demo.txt","pattern":"Bonjour","context":1}),
                cwd,
                None,
            )
            .title,
            "Find excerpts for `Bonjour` in test_tool_demo.txt"
        );

        assert_eq!(
            ToolInfo::build(
                "shell_exec",
                &serde_json::json!({"command":"echo 'Liste des outils demandée'"}),
                cwd,
                None,
            )
            .title,
            "echo 'Liste des outils demandée'"
        );
    }

    #[test]
    fn write_and_edit_are_diff_cards() {
        let cwd = Path::new("/tmp/project");

        for name in ["file_write", "file_edit", "replace_in_file"] {
            let info = ToolInfo::build(
                name,
                &serde_json::json!({
                    "path":"src/lib.rs",
                    "old_string":"a",
                    "new_string":"b",
                    "content":"fn main() {}",
                }),
                cwd,
                None,
            );
            assert_eq!(info.kind, agent_client_protocol::schema::v1::ToolKind::Edit);
            assert!(matches!(info.content.first(), Some(ToolCallContent::Diff(_))));
        }
    }

    #[test]
    fn shell_can_embed_terminal() {
        let info = ToolInfo::build(
            "shell_exec",
            &serde_json::json!({"command":"cargo test"}),
            Path::new("/tmp"),
            Some("term_1"),
        );
        assert!(matches!(info.content.first(), Some(ToolCallContent::Terminal(_))));
    }

    #[test]
    fn search_output_is_relative_and_line_aware() {
        let cwd = Path::new("/tmp/test-workspace");
        let raw = "/tmp/test-workspace/test_tool_demo.txt:2:Ligne 2\n/tmp/test-workspace/test_tool_demo.txt:3:Ligne 3";

        let rendered = normalize_search_result("search", raw, cwd);
        assert!(rendered.contains("test_tool_demo.txt:2:Ligne 2"));
        assert!(!rendered.contains("/tmp/test-workspace/test_tool_demo.txt"));

        let locations = search_result_locations(raw, cwd);
        assert_eq!(locations.len(), 2);
    }

    #[test]
    fn search_and_read_heading_is_relative() {
        let cwd = Path::new("/tmp/test-workspace");
        let raw = "## /tmp/test-workspace/test_tool_demo.txt:1\nBonjour\nLigne 2";

        let rendered = normalize_search_result("search_and_read", raw, cwd);
        assert!(rendered.starts_with("## test_tool_demo.txt:1"));
        assert!(!rendered.contains("/tmp/test-workspace/"));
    }

    #[test]
    fn large_input_is_bounded() {
        let args = serde_json::json!({"content":"x".repeat(MAX_RAW_INPUT_CHARS + 32)});
        let bounded = bounded_raw_input(&args);
        let text = bounded.get("content").and_then(|v| v.as_str()).unwrap();
        assert!(text.contains("chars omitted"));
        assert!(text.chars().count() > MAX_RAW_INPUT_CHARS);
    }
}
