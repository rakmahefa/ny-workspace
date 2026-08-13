//! Protocol-level ACP tool UX mapping for Gemini tools.
//!
//! All tools use one visual contract: identity, lifecycle, permission/risk,
//! then tool-specific payload. Tool-specific payloads never bypass the card.
//! The ACP wire status remains authoritative; this module only controls the
//! human-facing content attached to the tool call/update.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agent_client_protocol::schema::v1::{
    Content, ContentBlock, Diff, TextContent, ToolCallContent, ToolCallLocation, ToolCallStatus,
};
use serde_json::Value;

use super::lifecycle::ToolLifecycleState;
use super::sandbox::{RiskLevel, ShellAnalysis, ShellSandbox};

const MAX_DIFF_OLD_TEXT_BYTES: u64 = 64 * 1024;
const MAX_RAW_INPUT_CHARS: usize = 8 * 1024;
const MAX_RESULT_LOCATIONS: usize = 8;
const MAX_RESULT_PREVIEW_CHARS: usize = 4 * 1024;
const MAX_QUESTION_PREVIEW_CHARS: usize = 2 * 1024;

#[derive(Debug, Clone)]
pub struct ToolInfo {
    pub title: String,
    pub kind: agent_client_protocol::schema::v1::ToolKind,
    pub content: Vec<ToolCallContent>,
    pub locations: Vec<ToolCallLocation>,
}

impl ToolInfo {
    pub fn build(name: &str, args: &Value, cwd: &Path, terminal_id: Option<&str>) -> Self {
        match name {
            "file_read" => file_read(args, cwd),
            "file_write" => file_write(args, cwd),
            "file_edit" | "replace_in_file" => file_edit(args, cwd),
            "search" => search(args, cwd),
            "search_and_read" => search_and_read(args, cwd),
            "shell_exec" => shell_exec(args, terminal_id),
            "AskUserQuestion" => ask_user_question(args),
            _ => generic(name, args),
        }
    }
}

#[derive(Debug, Clone)]
struct ToolVisual {
    icon: &'static str,
    label: &'static str,
    permission: &'static str,
    risk: RiskLevel,
}

impl ToolVisual {
    fn for_tool(name: &str, args: &Value) -> Self {
        let (icon, label) = tool_visual(name);
        let permission = permission_label(name);
        let risk = classify_risk(name, args);
        Self { icon, label, permission, risk }
    }
}

/// Bound raw input before it reaches an ACP client.
pub fn bounded_raw_input(args: &Value) -> Value {
    let mut value = args.clone();
    let Some(object) = value.as_object_mut() else { return value };
    let Some(content_value) = object.get_mut("content") else { return value };
    let Some(content) = content_value.as_str() else { return value };

    let count = content.chars().count();
    if count <= MAX_RAW_INPUT_CHARS {
        return value;
    }

    let preview: String = content.chars().take(MAX_RAW_INPUT_CHARS).collect();
    *content_value = Value::String(format!(
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

/// Single visual contract used by every tool call and tool update.
fn ux_card(tool_name: &str, phase: &str, args: &Value, terminal: Option<&str>) -> ToolCallContent {
    let visual = ToolVisual::for_tool(tool_name, args);
    let terminal_suffix = terminal.map(|id| format!("  ·  ▣ terminal {id}")).unwrap_or_default();
    let text = format!(
        "{} {}  ·  {}  ·  {}  ·  {} {}{}{}",
        visual.icon,
        visual.label,
        phase,
        visual.permission,
        visual.risk.emoji(),
        visual.risk.label(),
        terminal_suffix,
        if terminal.is_some() { "" } else { "" }
    );
    text_content(&text, false)
}

fn tool_visual(name: &str) -> (&'static str, &'static str) {
    match name {
        "file_read" => ("📖", "File Read"),
        "file_write" => ("📝", "File Write"),
        "file_edit" | "replace_in_file" => ("✏️", "File Edit"),
        "search" | "search_and_read" => ("🔎", "Search"),
        "shell_exec" => ("▣", "Shell"),
        "AskUserQuestion" => ("⚙️", "Ask User"),
        _ => ("⚙️", "Tool"),
    }
}

fn permission_label(name: &str) -> &'static str {
    match name {
        "file_write" | "file_edit" | "replace_in_file" | "shell_exec" => "🔐 permission",
        "AskUserQuestion" => "👤 user input",
        _ => "🔓 no permission",
    }
}

pub fn result_update(
    tool_name: &str,
    args: &Value,
    result: &str,
    is_ok: bool,
    cwd: &Path,
    terminal_id: Option<&str>,
) -> ResultUpdate {
    let status = if is_ok { ToolCallStatus::Completed } else { ToolCallStatus::Failed };
    let phase = if is_ok { "🟢 completed" } else { "🔴 failed" };
    let header = ux_card(tool_name, phase, args, terminal_id);

    match tool_name {
        "file_read" => {
            if !is_ok {
                return ResultUpdate { status, content: vec![header, text_content(result, true)], locations: file_location(args, cwd) };
            }
            let start = args.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
            let numbered = result
                .trim_end_matches('\n')
                .split('\n')
                .enumerate()
                .map(|(idx, line)| format!("{}\t{}", start + idx, line))
                .collect::<Vec<_>>()
                .join("\n");
            ResultUpdate {
                status,
                content: if numbered.is_empty() { vec![header] } else { vec![header, text_content(&numbered, false)] },
                locations: file_location(args, cwd),
            }
        }
        "shell_exec" => {
            let mut content = vec![header];
            if let Some(id) = terminal_id {
                content.push(ToolCallContent::Terminal(agent_client_protocol::schema::v1::Terminal::new(id.to_owned())));
            } else {
                content.push(text_content(&format!("```console\n{}\n```", result.trim_end()), !is_ok));
            }
            ResultUpdate { status, content, locations: vec![] }
        }
        "file_write" | "file_edit" | "replace_in_file" => ResultUpdate {
            status,
            content: if is_ok { vec![header] } else { vec![header, text_content(result, true)] },
            locations: file_location(args, cwd),
        },
        "search" | "search_and_read" => {
            let locations = search_result_locations(result, cwd);
            let rendered = normalize_search_result(tool_name, result, cwd);
            ResultUpdate {
                status,
                content: if rendered.is_empty() { vec![header] } else { vec![header, text_content(&rendered, !is_ok)] },
                locations: if locations.is_empty() { search_location(args, cwd) } else { locations },
            }
        }
        "AskUserQuestion" => ResultUpdate {
            status,
            content: if is_ok {
                vec![header, text_content(&render_ask_user_result(result), false)]
            } else {
                vec![header, text_content(result, true)]
            },
            locations: vec![],
        },
        _ => ResultUpdate {
            status,
            content: if is_ok {
                if result.trim().is_empty() { vec![header] } else { vec![header, text_content(result, false)] }
            } else {
                vec![header, text_content(result, true)]
            },
            locations: vec![],
        },
    }
}

fn file_read(args: &Value, cwd: &Path) -> ToolInfo {
    let path = arg_str(args, "path").unwrap_or("File");
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1);
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(500).max(1);
    ToolInfo {
        title: format!("Read {} ({}-{})", display_path(path, cwd), offset, offset + limit - 1),
        kind: agent_client_protocol::schema::v1::ToolKind::Read,
        content: vec![ux_card("file_read", "⏳ pending", args, None)],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd)).line(offset as u32)],
    }
}

fn file_write(args: &Value, cwd: &Path) -> ToolInfo {
    let path = arg_str(args, "path").unwrap_or("File");
    let content = arg_str(args, "content").unwrap_or("");
    let resolved = resolve_path(path, cwd);
    let diff = Diff::new(resolved.clone(), content.to_owned()).old_text(read_old_text(&resolved));
    ToolInfo {
        title: format!("Write {}", display_path(path, cwd)),
        kind: agent_client_protocol::schema::v1::ToolKind::Edit,
        content: vec![ux_card("file_write", "⏳ pending", args, None), ToolCallContent::Diff(diff)],
        locations: vec![ToolCallLocation::new(resolved)],
    }
}

fn file_edit(args: &Value, cwd: &Path) -> ToolInfo {
    let path = arg_str(args, "path").unwrap_or("File");
    let old = arg_str(args, "old_string").unwrap_or("");
    let new = arg_str(args, "new_string").unwrap_or("");
    let resolved = resolve_path(path, cwd);
    let old_text = if old.is_empty() { read_old_text(&resolved) } else { Some(old.to_owned()) };
    let diff = Diff::new(resolved.clone(), new.to_owned()).old_text(old_text);
    ToolInfo {
        title: format!("Edit {}", display_path(path, cwd)),
        kind: agent_client_protocol::schema::v1::ToolKind::Edit,
        content: vec![ux_card("file_edit", "⏳ pending", args, None), ToolCallContent::Diff(diff)],
        locations: vec![ToolCallLocation::new(resolved)],
    }
}

fn search(args: &Value, cwd: &Path) -> ToolInfo {
    let pattern = arg_str(args, "pattern").unwrap_or("");
    let path = arg_str(args, "path").unwrap_or(".");
    ToolInfo {
        title: if path == "." { format!("Find `{}`", truncate(pattern, 72)) } else { format!("Find `{}` in {}", truncate(pattern, 56), display_path(path, cwd)) },
        kind: agent_client_protocol::schema::v1::ToolKind::Search,
        content: vec![ux_card("search", "⏳ pending", args, None)],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd))],
    }
}

fn search_and_read(args: &Value, cwd: &Path) -> ToolInfo {
    let pattern = arg_str(args, "pattern").unwrap_or("");
    let path = arg_str(args, "path").unwrap_or(".");
    ToolInfo {
        title: if path == "." { format!("Find excerpts for `{}`", truncate(pattern, 56)) } else { format!("Find excerpts for `{}` in {}", truncate(pattern, 40), display_path(path, cwd)) },
        kind: agent_client_protocol::schema::v1::ToolKind::Search,
        content: vec![ux_card("search_and_read", "⏳ pending", args, None)],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd))],
    }
}

fn shell_exec(args: &Value, terminal_id: Option<&str>) -> ToolInfo {
    let command = arg_str(args, "command").unwrap_or("");
    let mut content = vec![ux_card("shell_exec", "⏳ pending", args, terminal_id)];
    if let Some(id) = terminal_id {
        content.push(ToolCallContent::Terminal(agent_client_protocol::schema::v1::Terminal::new(id.to_owned())));
    }
    ToolInfo {
        title: if command.is_empty() { "Terminal".into() } else { truncate(command, 96) },
        kind: agent_client_protocol::schema::v1::ToolKind::Execute,
        content,
        locations: vec![],
    }
}

fn ask_user_question(args: &Value) -> ToolInfo {
    let title = ask_user_title(args);
    ToolInfo {
        title,
        kind: agent_client_protocol::schema::v1::ToolKind::Other,
        content: vec![
            ux_card("AskUserQuestion", "⏳ waiting for user", args, None),
            text_content(&render_ask_user_input(args), false),
        ],
        locations: vec![],
    }
}

fn generic(name: &str, args: &Value) -> ToolInfo {
    ToolInfo {
        title: name.to_owned(),
        kind: agent_client_protocol::schema::v1::ToolKind::Other,
        content: if args.as_object().is_none_or(|obj| obj.is_empty()) {
            vec![ux_card(name, "⏳ pending", args, None)]
        } else {
            vec![ux_card(name, "⏳ pending", args, None), text_content(&concise_args(args), false)]
        },
        locations: vec![],
    }
}

fn text_content(text: &str, error: bool) -> ToolCallContent {
    let rendered = if error { format!("```text\n{text}\n```") } else { text.to_owned() };
    ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(rendered))))
}

fn render_ask_user_input(args: &Value) -> String {
    let Some(questions) = args.get("questions").and_then(Value::as_array) else {
        return "Question indisponible.".into();
    };
    let mut output = String::new();
    for (index, question) in questions.iter().enumerate() {
        if index > 0 { output.push_str("\n\n"); }
        let header = question.get("header").and_then(Value::as_str).unwrap_or("Question");
        let prompt = question.get("question").and_then(Value::as_str).unwrap_or("Question indisponible.");
        output.push_str(&format!("**{header}**\n{prompt}"));
        if let Some(options) = question.get("options").and_then(Value::as_array) {
            for option in options {
                if let Some(label) = option.get("label").and_then(Value::as_str) {
                    output.push_str(&format!("\n- {label}"));
                }
            }
        }
    }
    truncate(&output, MAX_QUESTION_PREVIEW_CHARS)
}

fn render_ask_user_result(result: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(result) else { return result.to_owned() };
    let Some(answers) = value.get("answers").and_then(Value::as_object) else { return result.to_owned() };
    if answers.is_empty() { return "Aucune réponse sélectionnée.".into(); }
    answers
        .iter()
        .map(|(question, answer)| format!("**{question}**\n{}", answer_display(answer)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn answer_display(value: &Value) -> String {
    match value {
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

fn ask_user_title(args: &Value) -> String {
    let question = args
        .get("questions")
        .and_then(Value::as_array)
        .and_then(|questions| questions.first())
        .and_then(|question| question.get("question"))
        .and_then(Value::as_str)
        .unwrap_or("User input");
    format!("Ask user · {}", truncate(question, 72))
}

fn file_location(args: &Value, cwd: &Path) -> Vec<ToolCallLocation> {
    arg_str(args, "path").map(|path| vec![ToolCallLocation::new(resolve_path(path, cwd))]).unwrap_or_default()
}

fn search_location(args: &Value, cwd: &Path) -> Vec<ToolCallLocation> {
    let path = arg_str(args, "path").unwrap_or(".");
    vec![ToolCallLocation::new(resolve_path(path, cwd))]
}

fn normalize_search_result(tool_name: &str, result: &str, cwd: &Path) -> String {
    let mut output = String::with_capacity(result.len().min(MAX_RESULT_PREVIEW_CHARS));
    for (index, line) in result.lines().enumerate() {
        if output.chars().count() >= MAX_RESULT_PREVIEW_CHARS { break; }
        if index > 0 { output.push('\n'); }
        if tool_name == "search_and_read" && line.starts_with("## ") {
            output.push_str(&normalize_heading_path(line, cwd));
        } else {
            output.push_str(&normalize_match_line(line, cwd));
        }
    }
    truncate(&output, MAX_RESULT_PREVIEW_CHARS)
}

fn normalize_heading_path(line: &str, cwd: &Path) -> String {
    let body = &line[3..];
    let Some((path, line_number, tail)) = split_path_line(body) else { return line.to_owned() };
    format!("## {}:{}{}", display_path(path, cwd), line_number, tail)
}

fn normalize_match_line(line: &str, cwd: &Path) -> String {
    let Some((path, line_number, tail)) = split_path_line(line) else { return line.to_owned() };
    if path.starts_with('(') || path.starts_with('…') { return line.to_owned(); }
    format!("{}:{}{}", display_path(path, cwd), line_number, tail)
}

fn split_path_line(line: &str) -> Option<(&str, u32, &str)> {
    let first_colon = line.find(':')?;
    let path = &line[..first_colon];
    if path.is_empty() { return None; }
    let after_path = &line[first_colon + 1..];
    let digit_len = after_path.chars().take_while(|c| c.is_ascii_digit()).count();
    if digit_len == 0 { return None; }
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
        if locations.len() >= MAX_RESULT_LOCATIONS { break; }
    }
    locations
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
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

fn concise_args(args: &Value) -> String {
    let Some(obj) = args.as_object() else { return "{}".into() };
    format!("Arguments: {}", obj.keys().cloned().collect::<Vec<_>>().join(", "))
}

pub fn classify_risk(name: &str, args: &Value) -> RiskLevel {
    match name {
        "shell_exec" => arg_str(args, "command")
            .and_then(|command| ShellSandbox::new().analyze_command(command).ok())
            .map(|ShellAnalysis { risk, .. }| risk)
            .unwrap_or(RiskLevel::Critical),
        "file_write" | "file_edit" | "replace_in_file" => RiskLevel::Medium,
        _ => RiskLevel::Low,
    }
}

pub fn lifecycle_label(state: ToolLifecycleState) -> &'static str {
    match state {
        ToolLifecycleState::Pending => "pending",
        ToolLifecycleState::Permission => "permission",
        ToolLifecycleState::Executing => "executing",
        ToolLifecycleState::Completed => "completed",
        ToolLifecycleState::Failed => "failed",
        ToolLifecycleState::Cancelled => "cancelled",
    }
}

pub fn lifecycle_icon(state: ToolLifecycleState) -> &'static str {
    match state {
        ToolLifecycleState::Pending => "⏳",
        ToolLifecycleState::Permission => "🔐",
        ToolLifecycleState::Executing => "▶",
        ToolLifecycleState::Completed => "🟢",
        ToolLifecycleState::Failed => "🔴",
        ToolLifecycleState::Cancelled => "⚪",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_core_tool_has_one_consistent_card_first() {
        let cwd = Path::new("/tmp/project");
        for (name, args) in [
            ("file_read", serde_json::json!({"path":"src/lib.rs"})),
            ("file_write", serde_json::json!({"path":"src/lib.rs","content":"x"})),
            ("file_edit", serde_json::json!({"path":"src/lib.rs","old_string":"a","new_string":"b"})),
            ("search", serde_json::json!({"pattern":"foo","path":"src"})),
            ("search_and_read", serde_json::json!({"pattern":"foo","path":"src"})),
            ("shell_exec", serde_json::json!({"command":"cargo test"})),
            ("AskUserQuestion", serde_json::json!({"questions":[{"header":"Confirm","question":"Continue?","options":[{"label":"Yes"}]}]})),
        ] {
            let info = ToolInfo::build(name, &args, cwd, None);
            assert!(matches!(info.content.first(), Some(ToolCallContent::Content(_))), "missing UX card for {name}");
        }
    }

    #[test]
    fn ask_user_is_not_raw_json() {
        let args = serde_json::json!({
            "questions": [{"header":"Cleanup","question":"Keep the file?","options":[{"label":"Keep"},{"label":"Delete"}]}]
        });
        let info = ToolInfo::build("AskUserQuestion", &args, Path::new("/tmp"), None);
        assert!(info.title.starts_with("Ask user · Cleanup"));
        let rendered = match &info.content[1] { ToolCallContent::Content(content) => format!("{:?}", content), _ => String::new() };
        assert!(!rendered.contains("\"questions\""));
    }

    #[test]
    fn ask_user_result_is_human_readable() {
        let result = serde_json::json!({"answers":{"Cleanup":"Delete"}}).to_string();
        assert_eq!(render_ask_user_result(&result), "**Cleanup**\nDelete");
    }

    #[test]
    fn write_and_edit_keep_diff_cards() {
        let cwd = Path::new("/tmp/project");
        for name in ["file_write", "file_edit", "replace_in_file"] {
            let info = ToolInfo::build(name, &serde_json::json!({"path":"src/lib.rs","old_string":"a","new_string":"b","content":"fn main() {}"}), cwd, None);
            assert_eq!(info.kind, agent_client_protocol::schema::v1::ToolKind::Edit);
            assert!(info.content.iter().any(|item| matches!(item, ToolCallContent::Diff(_))));
        }
    }

    #[test]
    fn shell_card_can_embed_terminal() {
        let info = ToolInfo::build("shell_exec", &serde_json::json!({"command":"cargo test"}), Path::new("/tmp"), Some("term_1"));
        assert!(matches!(info.content.first(), Some(ToolCallContent::Content(_))));
        assert!(info.content.iter().any(|item| matches!(item, ToolCallContent::Terminal(_))));
    }

    #[test]
    fn result_is_consistent_across_tools() {
        let cwd = Path::new("/tmp/test-workspace");
        for name in ["file_read", "file_write", "file_edit", "search", "AskUserQuestion"] {
            let args = match name {
                "file_read" => serde_json::json!({"path":"a.txt"}),
                "file_write" => serde_json::json!({"path":"a.txt","content":"x"}),
                "file_edit" => serde_json::json!({"path":"a.txt","old_string":"a","new_string":"b"}),
                "search" => serde_json::json!({"pattern":"x","path":"."}),
                _ => serde_json::json!({"questions":[{"question":"Keep?","options":[{"label":"Yes"}]}]}),
            };
            let update = result_update(name, &args, if name == "AskUserQuestion" { r#"{"answers":{"Keep?":"Yes"}}"# } else { "ok" }, true, cwd, None);
            assert_eq!(update.status, ToolCallStatus::Completed);
            assert!(matches!(update.content.first(), Some(ToolCallContent::Content(_))), "missing completion card for {name}");
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
        let text = bounded.get("content").and_then(Value::as_str).unwrap();
        assert!(text.contains("chars omitted"));
        assert!(text.chars().count() > MAX_RAW_INPUT_CHARS);
    }
}
