//! Visual ACP tool UX for Gemini tools.
//!
//! The transport still speaks standard ACP. This module concentrates the
//! presentation layer so file, edit, search, shell and interactive tools read
//! like compact IDE cards rather than raw JSON-RPC payloads.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    Content, ContentBlock, Diff, TextContent, ToolCallContent, ToolCallLocation, ToolCallStatus,
};

use super::sandbox::{RiskLevel, ShellAnalysis, ShellSandbox};

const MAX_DIFF_OLD_TEXT_BYTES: u64 = 64 * 1024;
const MAX_RAW_INPUT_CHARS: usize = 8 * 1024;
const MAX_RESULT_LOCATIONS: usize = 8;
const MAX_PREVIEW_LINES: usize = 80;

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
            "AskUserQuestion" | "ask_user_question" => ask_user_question(args),
            _ => generic(name, args),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResultUpdate {
    pub status: ToolCallStatus,
    pub content: Vec<ToolCallContent>,
    pub locations: Vec<ToolCallLocation>,
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
        "file_write" | "file_edit" | "replace_in_file" => edit_result(args, result, is_ok, cwd),
        "search" | "search_and_read" => search_result(tool_name, args, result, is_ok, cwd),
        "shell_exec" => shell_result(result, is_ok, terminal_id),
        "AskUserQuestion" | "ask_user_question" => interactive_result(result, is_ok),
        _ => ResultUpdate {
            status: status_for(is_ok),
            content: text_content(result, !is_ok),
            locations: vec![],
        },
    }
}

fn file_read(args: &serde_json::Value, cwd: &Path) -> ToolInfo {
    let path = arg_str(args, "path").unwrap_or("File");
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1).max(1);
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(500).max(1);
    let end = offset.saturating_add(limit).saturating_sub(1);

    ToolInfo {
        title: format!("📖 file_read · {}", display_path(path, cwd)),
        kind: agent_client_protocol::schema::v1::ToolKind::Read,
        content: vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
            TextContent::new(format!("Lines {offset}-{end}")),
        )))],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd)).line(offset as u32)],
    }
}

fn file_read_result(
    args: &serde_json::Value,
    result: &str,
    is_ok: bool,
    cwd: &Path,
) -> ResultUpdate {
    let locations = file_location(args, cwd);
    if !is_ok {
        return ResultUpdate {
            status: ToolCallStatus::Failed,
            content: text_content(&format!("\n🔴 File read failed\n{result}"), true),
            locations,
        };
    }

    let start = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
    let preview = numbered_preview(result, start, MAX_PREVIEW_LINES);
    let line_count = result.lines().count();
    let footer = if line_count > MAX_PREVIEW_LINES {
        format!("\n\n🟢 File Loaded · {line_count} lines read · preview truncated")
    } else {
        "\n\n🟢 File Loaded".to_owned()
    };

    ResultUpdate {
        status: ToolCallStatus::Completed,
        content: text_content(&(preview + &footer), false),
        locations,
    }
}

fn file_write(args: &serde_json::Value, cwd: &Path) -> ToolInfo {
    let path = arg_str(args, "path").unwrap_or("File");
    let content = arg_str(args, "content").unwrap_or("");
    let resolved = resolve_path(path, cwd);
    let diff = Diff::new(resolved.clone(), content.to_owned()).old_text(read_old_text(&resolved));

    ToolInfo {
        title: format!("✏️ file_write · {}", display_path(path, cwd)),
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
        title: format!("✏️ file_edit · {}", display_path(path, cwd)),
        kind: agent_client_protocol::schema::v1::ToolKind::Edit,
        content: vec![ToolCallContent::Diff(diff)],
        locations: vec![ToolCallLocation::new(resolved)],
    }
}

fn edit_result(
    args: &serde_json::Value,
    result: &str,
    is_ok: bool,
    cwd: &Path,
) -> ResultUpdate {
    let path = display_path(arg_str(args, "path").unwrap_or("file"), cwd);
    let summary = summarize_edit_result(result, is_ok);
    ResultUpdate {
        status: status_for(is_ok),
        content: if is_ok {
            text_content(&format!("🟢 {summary}"), false)
        } else {
            text_content(&format!("🔴 {path}\n{result}"), true)
        },
        locations: file_location(args, cwd),
    }
}

fn search(args: &serde_json::Value, cwd: &Path) -> ToolInfo {
    let pattern = arg_str(args, "pattern").unwrap_or("");
    let path = arg_str(args, "path").unwrap_or(".");
    let scope = display_path(path, cwd);
    let title = if scope == "." {
        format!("🔍 search · \"{}\"", truncate(pattern, 72))
    } else {
        format!("🔍 search · \"{}\" · {}", truncate(pattern, 56), scope)
    };

    ToolInfo {
        title,
        kind: agent_client_protocol::schema::v1::ToolKind::Search,
        content: vec![],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd))],
    }
}

fn search_and_read(args: &serde_json::Value, cwd: &Path) -> ToolInfo {
    let pattern = arg_str(args, "pattern").unwrap_or("");
    let path = arg_str(args, "path").unwrap_or(".");
    let scope = display_path(path, cwd);
    let title = if scope == "." {
        format!("🔍 search_and_read · \"{}\"", truncate(pattern, 56))
    } else {
        format!(
            "🔍 search_and_read · \"{}\" · {}",
            truncate(pattern, 42),
            scope
        )
    };

    ToolInfo {
        title,
        kind: agent_client_protocol::schema::v1::ToolKind::Search,
        content: vec![],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd))],
    }
}

fn search_result(
    tool_name: &str,
    args: &serde_json::Value,
    result: &str,
    is_ok: bool,
    cwd: &Path,
) -> ResultUpdate {
    let locations = search_result_locations(result, cwd);
    let displayed = if locations.is_empty() {
        search_location(args, cwd)
    } else {
        locations
    };

    if !is_ok {
        return ResultUpdate {
            status: ToolCallStatus::Failed,
            content: text_content(&format!("🔴 Search failed\n{result}"), true),
            locations: displayed,
        };
    }

    let count = search_match_count(result);
    let body = normalize_search_result(tool_name, result, cwd);
    let footer = format!("\n\n🟢 {count} matches read");

    ResultUpdate {
        status: ToolCallStatus::Completed,
        content: text_content(&(body + &footer), false),
        locations: displayed,
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
        title: format!("⚡ shell_exec · {}", truncate(command, 96)),
        kind: agent_client_protocol::schema::v1::ToolKind::Execute,
        content,
        locations: vec![],
    }
}

fn shell_result(result: &str, is_ok: bool, terminal_id: Option<&str>) -> ResultUpdate {
    let exit_code = extract_exit_code(result, is_ok);
    let footer = match exit_code {
        Some(code) if code == 0 => format!("\n\n🟢 Exit code: {code}"),
        Some(code) => format!("\n\n🔴 Exit code: {code}"),
        None if is_ok => "\n\n🟢 Command completed".to_owned(),
        None => "\n\n🔴 Command failed".to_owned(),
    };

    let content = if terminal_id.is_some() {
        vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
            TextContent::new(footer.trim().to_owned()),
        )))]
    } else {
        text_content(&format!("```console\n{}\n```{footer}", result.trim_end()), !is_ok)
    };

    ResultUpdate {
        status: status_for(is_ok),
        content,
        locations: vec![],
    }
}

fn ask_user_question(args: &serde_json::Value) -> ToolInfo {
    let question = extract_question(args).unwrap_or_else(|| "Question utilisateur".to_owned());
    ToolInfo {
        title: "❓ AskUserQuestion".to_owned(),
        kind: agent_client_protocol::schema::v1::ToolKind::Other,
        content: text_content(&format!("💬 \"{}\"\n\n⏳ Awaiting Input", truncate(&question, 240)), false),
        locations: vec![],
    }
}

fn interactive_result(result: &str, is_ok: bool) -> ResultUpdate {
    if is_ok {
        ResultUpdate {
            status: ToolCallStatus::Completed,
            content: text_content(&format!("🟢 Input received\n{result}"), false),
            locations: vec![],
        }
    } else {
        ResultUpdate {
            status: ToolCallStatus::Failed,
            content: text_content(&format!("🔴 Input cancelled or failed\n{result}"), true),
            locations: vec![],
        }
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

fn status_for(is_ok: bool) -> ToolCallStatus {
    if is_ok {
        ToolCallStatus::Completed
    } else {
        ToolCallStatus::Failed
    }
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

fn numbered_preview(result: &str, start: usize, max_lines: usize) -> String {
    result
        .lines()
        .take(max_lines)
        .enumerate()
        .map(|(idx, line)| format!("{:>4} │ {}", start + idx, line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn summarize_edit_result(result: &str, is_ok: bool) -> String {
    if !is_ok {
        return "Edit failed".to_owned();
    }
    let lower = result.to_ascii_lowercase();
    if lower.contains("replacement") || lower.contains("replaced") {
        let number = lower
            .split_whitespace()
            .find_map(|token| token.parse::<usize>().ok())
            .unwrap_or(1);
        return if number == 1 {
            "1 Replacement".to_owned()
        } else {
            format!("{number} Replacements")
        };
    }
    "File Updated".to_owned()
}

fn search_match_count(result: &str) -> usize {
    result
        .lines()
        .filter(|line| split_path_line(line).is_some() || line.starts_with("## "))
        .count()
        .max(if result.trim().is_empty() { 0 } else { 1 })
}

fn extract_exit_code(result: &str, is_ok: bool) -> Option<i32> {
    let needle = "exit code";
    result
        .to_ascii_lowercase()
        .split(needle)
        .nth(1)
        .and_then(|tail| tail.split_whitespace().find_map(|token| token.trim_matches(|c: char| !c.is_ascii_digit() && c != '-').parse::<i32>().ok()))
        .or_else(|| if is_ok { Some(0) } else { None })
}

fn extract_question(args: &serde_json::Value) -> Option<String> {
    if let Some(question) = args.get("question").and_then(serde_json::Value::as_str) {
        return Some(question.to_owned());
    }
    let questions = args.get("questions")?.as_array()?;
    questions
        .first()
        .and_then(|item| item.get("question").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

fn normalize_search_result(tool_name: &str, result: &str, cwd: &Path) -> String {
    let mut output = String::with_capacity(result.len());
    for (index, line) in result.lines().enumerate() {
        if index > 0 {
            output.push('\n');
        }
        if tool_name == "search_and_read" && line.starts_with("## ") {
            output.push_str(&normalize_heading_path(line, cwd));
        } else {
            output.push_str(&normalize_match_line(line, cwd));
        }
    }
    output
}

fn normalize_heading_path(line: &str, cwd: &Path) -> String {
    let body = &line[3..];
    let Some((path, line_number, tail)) = split_path_line(body) else {
        return line.to_owned();
    };
    format!("## {}:{}{}", display_path(path, cwd), line_number, tail)
}

fn normalize_match_line(line: &str, cwd: &Path) -> String {
    let Some((path, line_number, tail)) = split_path_line(line) else {
        return line.to_owned();
    };
    format!("{}:{}{}", display_path(path, cwd), line_number, tail)
}

fn split_path_line(line: &str) -> Option<(&str, u32, &str)> {
    let first_colon = line.find(':')?;
    let path = &line[..first_colon];
    if path.is_empty() {
        return None;
    }
    let after_path = &line[first_colon + 1..];
    let digit_len = after_path.chars().take_while(|c| c.is_ascii_digit()).count();
    if digit_len == 0 {
        return None;
    }
    let line_number = after_path[..digit_len].parse::<u32>().ok()?;
    Some((path, line_number, &after_path[digit_len..]))
}

fn search_result_locations(result: &str, cwd: &Path) -> Vec<ToolCallLocation> {
    let mut locations = Vec::new();
    let mut seen = BTreeSet::new();

    for line in result.lines() {
        let candidate = line.strip_prefix("## ").unwrap_or(line);
        let Some((path, line_number, _)) = split_path_line(candidate) else { continue };
        let resolved = resolve_path(path, cwd);
        let key = format!("{}:{line_number}", resolved.display());
        if seen.insert(key) {
            locations.push(ToolCallLocation::new(resolved).line(line_number));
        }
        if locations.len() >= MAX_RESULT_LOCATIONS {
            break;
        }
    }
    locations
}

fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(serde_json::Value::as_str)
}

fn resolve_path(path: &str, cwd: &Path) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() { candidate } else { cwd.join(candidate) }
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
    if metadata.len() > MAX_DIFF_OLD_TEXT_BYTES { return None; }
    std::fs::read_to_string(path).ok()
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max { return value.to_owned(); }
    format!("{}…", value.chars().take(max).collect::<String>())
}

fn concise_args(args: &serde_json::Value) -> String {
    let Some(obj) = args.as_object() else { return "{}".into() };
    format!("Arguments: {}", obj.keys().cloned().collect::<Vec<_>>().join(", "))
}

pub fn classify_risk(name: &str, args: &serde_json::Value) -> RiskLevel {
    match name {
        "shell_exec" => arg_str(args, "command")
            .and_then(|command| ShellSandbox::new().analyze_command(command).ok())
            .map(|ShellAnalysis { risk, .. }| risk)
            .unwrap_or(RiskLevel::Critical),
        "file_write" | "file_edit" | "replace_in_file" => RiskLevel::Medium,
        "AskUserQuestion" | "ask_user_question" => RiskLevel::Medium,
        _ => RiskLevel::Low,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visual_titles_match_requested_layout() {
        let cwd = Path::new("/tmp/test-workspace");
        assert_eq!(
            ToolInfo::build("file_read", &serde_json::json!({"path":"src/index.ts","offset":1,"limit":2}), cwd, None).title,
            "📖 file_read · src/index.ts"
        );
        assert_eq!(
            ToolInfo::build("file_write", &serde_json::json!({"path":"src/server.ts","content":"const PORT = 3000;"}), cwd, None).title,
            "✏️ file_write · src/server.ts"
        );
        assert_eq!(
            ToolInfo::build("search", &serde_json::json!({"pattern":"connectDB","path":"src"}), cwd, None).title,
            "🔍 search · \"connectDB\" · src"
        );
        assert_eq!(
            ToolInfo::build("shell_exec", &serde_json::json!({"command":"pnpm test"}), cwd, None).title,
            "⚡ shell_exec · pnpm test"
        );
        assert_eq!(ToolInfo::build("AskUserQuestion", &serde_json::json!({"question":"Migrations en prod ?"}), cwd, None).title, "❓ AskUserQuestion");
    }

    #[test]
    fn read_preview_is_line_numbered() {
        let preview = numbered_preview("import x\nconst y = 1", 1, 10);
        assert!(preview.contains("   1 │ import x"));
        assert!(preview.contains("   2 │ const y = 1"));
    }

    #[test]
    fn write_and_edit_are_diff_cards() {
        let cwd = Path::new("/tmp/project");
        for name in ["file_write", "file_edit", "replace_in_file"] {
            let info = ToolInfo::build(name, &serde_json::json!({"path":"src/lib.rs","old_string":"a","new_string":"b","content":"fn main() {}"}), cwd, None);
            assert_eq!(info.kind, agent_client_protocol::schema::v1::ToolKind::Edit);
            assert!(matches!(info.content.first(), Some(ToolCallContent::Diff(_))));
        }
    }

    #[test]
    fn search_output_is_relative_and_line_aware() {
        let cwd = Path::new("/tmp/test-workspace");
        let raw = "/tmp/test-workspace/test_tool_demo.txt:2:Ligne 2\n/tmp/test-workspace/test_tool_demo.txt:3:Ligne 3";
        let rendered = normalize_search_result("search", raw, cwd);
        assert_eq!(rendered, "test_tool_demo.txt:2:Ligne 2\ntest_tool_demo.txt:3:Ligne 3");
        assert_eq!(search_result_locations(raw, cwd).len(), 2);
    }

    #[test]
    fn large_input_is_bounded() {
        let args = serde_json::json!({"content":"x".repeat(MAX_RAW_INPUT_CHARS + 32)});
        let bounded = bounded_raw_input(&args);
        let text = bounded.get("content").and_then(|v| v.as_str()).unwrap();
        assert!(text.contains("chars omitted"));
    }
}
