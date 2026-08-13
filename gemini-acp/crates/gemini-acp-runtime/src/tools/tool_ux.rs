//! Protocol-level ACP tool UX mapping for Gemini tools.
//!
//! Every tool follows the same visual contract:
//! 1. identity
//! 2. lifecycle / permission / risk metadata
//! 3. a formatted content or output section
//!
//! The ACP wire status remains authoritative; this module controls only the
//! human-facing content attached to tool calls and updates.

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
const MAX_CARD_BODY_CHARS: usize = 8 * 1024;

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
            "glob" => glob(args, cwd),
            "list_directory" => list_directory(args, cwd),
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
        Self {
            icon,
            label,
            permission: permission_label(name),
            risk: classify_risk(name, args),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum CardBodyKind {
    Output,
    Content,
    Input,
}

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

fn ux_card(
    tool_name: &str,
    phase: &str,
    args: &Value,
    body: Option<(&str, CardBodyKind, bool)>,
    terminal: Option<&str>,
) -> ToolCallContent {
    let visual = ToolVisual::for_tool(tool_name, args);
    let mut text = format!(
        "**{} {}**\n{}  ·  {}  ·  {} {}",
        visual.icon,
        visual.label,
        phase,
        visual.permission,
        visual.risk.emoji(),
        visual.risk.label(),
    );

    if let Some(terminal) = terminal {
        text.push_str(&format!("  ·  ▣ terminal {terminal}"));
    }

    let body = body
        .map(|(body, kind, error)| render_card_body(body, kind, error))
        .unwrap_or_else(|| render_card_body("_En attente du résultat…_", CardBodyKind::Content, false));

    text.push_str("\n\n");
    text.push_str(&body);
    text_content(&truncate(&text, MAX_CARD_BODY_CHARS), false)
}

fn render_card_body(body: &str, kind: CardBodyKind, error: bool) -> String {
    if body.trim().is_empty() {
        return match kind {
            CardBodyKind::Output => "**Output**\n_Sortie vide._".into(),
            CardBodyKind::Content => "**Content**\n_Aucun contenu._".into(),
            CardBodyKind::Input => "**Input**\n_Aucune donnée._".into(),
        };
    }

    let label = match kind {
        CardBodyKind::Output => "Output",
        CardBodyKind::Content => "Content",
        CardBodyKind::Input => "Input",
    };
    let _ = error;
    format!("**{label}**\n```text\n{body}\n```")
}

fn tool_visual(name: &str) -> (&'static str, &'static str) {
    match name {
        "file_read" => ("📖", "File Read"),
        "file_write" => ("📝", "File Write"),
        "file_edit" | "replace_in_file" => ("✏️", "File Edit"),
        "glob" => ("🧭", "Glob"),
        "list_directory" => ("📁", "Directory"),
        "search" => ("🔎", "Search"),
        "search_and_read" => ("🔎", "Search & Read"),
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

    match tool_name {
        "file_read" => {
            let body = if is_ok { format_numbered_read(result, args) } else { result.to_owned() };
            ResultUpdate {
                status,
                content: vec![ux_card(tool_name, phase, args, Some((&body, CardBodyKind::Output, !is_ok)), terminal_id)],
                locations: file_location(args, cwd),
            }
        }
        "glob" | "list_directory" => ResultUpdate {
            status,
            content: vec![ux_card(tool_name, phase, args, Some((result.trim_end(), CardBodyKind::Output, !is_ok)), terminal_id)],
            locations: filesystem_result_locations(tool_name, result, cwd),
        },
        "shell_exec" => {
            let mut content = vec![ux_card(tool_name, phase, args, Some((result.trim_end(), CardBodyKind::Output, !is_ok)), terminal_id)];
            if let Some(id) = terminal_id {
                content.push(ToolCallContent::Terminal(agent_client_protocol::schema::v1::Terminal::new(id.to_owned())));
            }
            ResultUpdate { status, content, locations: vec![] }
        }
        "file_write" | "file_edit" | "replace_in_file" => ResultUpdate {
            status,
            content: vec![ux_card(tool_name, phase, args, Some((result.trim_end(), CardBodyKind::Output, !is_ok)), terminal_id)],
            locations: file_location(args, cwd),
        },
        "search" | "search_and_read" => {
            let rendered = normalize_search_result(tool_name, result, cwd);
            ResultUpdate {
                status,
                content: vec![ux_card(tool_name, phase, args, Some((rendered.trim_end(), CardBodyKind::Output, !is_ok)), terminal_id)],
                locations: search_result_locations(result, cwd),
            }
        }
        "AskUserQuestion" => {
            let body = if is_ok { render_ask_user_result(result) } else { result.to_owned() };
            ResultUpdate {
                status,
                content: vec![ux_card(tool_name, phase, args, Some((&body, CardBodyKind::Content, !is_ok)), terminal_id)],
                locations: vec![],
            }
        }
        _ => ResultUpdate {
            status,
            content: vec![ux_card(tool_name, phase, args, Some((result.trim_end(), CardBodyKind::Output, !is_ok)), terminal_id)],
            locations: vec![],
        },
    }
}

fn format_numbered_read(result: &str, args: &Value) -> String {
    let start = args.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
    result
        .trim_end_matches('\n')
        .split('\n')
        .enumerate()
        .map(|(idx, line)| format!("{}\t{}", start + idx, line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn file_read(args: &Value, cwd: &Path) -> ToolInfo {
    let path = arg_str(args, "path").unwrap_or("File");
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1);
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(500).max(1);
    let input = format!("{}  ·  lignes {}-{}", display_path(path, cwd), offset, offset + limit - 1);
    ToolInfo {
        title: format!("Read {} ({}-{})", display_path(path, cwd), offset, offset + limit - 1),
        kind: agent_client_protocol::schema::v1::ToolKind::Read,
        content: vec![ux_card("file_read", "⏳ pending", args, Some((&input, CardBodyKind::Input, false)), None)],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd)).line(offset as u32)],
    }
}

fn file_write(args: &Value, cwd: &Path) -> ToolInfo {
    let path = arg_str(args, "path").unwrap_or("File");
    let content = arg_str(args, "content").unwrap_or("");
    let resolved = resolve_path(path, cwd);
    let diff = Diff::new(resolved.clone(), content.to_owned()).old_text(read_old_text(&resolved));
    let input = format!("{}  ·  {} chars", display_path(path, cwd), content.chars().count());
    ToolInfo {
        title: format!("Write {}", display_path(path, cwd)),
        kind: agent_client_protocol::schema::v1::ToolKind::Edit,
        content: vec![ux_card("file_write", "⏳ pending", args, Some((&input, CardBodyKind::Input, false)), None), ToolCallContent::Diff(diff)],
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
    let input = format!("{}  ·  replacement {} → {} chars", display_path(path, cwd), old.chars().count(), new.chars().count());
    ToolInfo {
        title: format!("Edit {}", display_path(path, cwd)),
        kind: agent_client_protocol::schema::v1::ToolKind::Edit,
        content: vec![ux_card("file_edit", "⏳ pending", args, Some((&input, CardBodyKind::Input, false)), None), ToolCallContent::Diff(diff)],
        locations: vec![ToolCallLocation::new(resolved)],
    }
}

fn glob(args: &Value, cwd: &Path) -> ToolInfo {
    let pattern = arg_str(args, "pattern").unwrap_or("");
    let path = arg_str(args, "path").unwrap_or(".");
    let max_results = args.get("max_results").and_then(Value::as_u64).unwrap_or(100);
    let input = format!("pattern `{}`  ·  path {}  ·  max {}", truncate(pattern, 72), display_path(path, cwd), max_results);
    ToolInfo {
        title: format!("Find paths `{}`", truncate(pattern, 72)),
        kind: agent_client_protocol::schema::v1::ToolKind::Search,
        content: vec![ux_card("glob", "⏳ pending", args, Some((&input, CardBodyKind::Input, false)), None)],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd))],
    }
}

fn list_directory(args: &Value, cwd: &Path) -> ToolInfo {
    let path = arg_str(args, "path").unwrap_or(".");
    let input = format!("path {}", display_path(path, cwd));
    ToolInfo {
        title: format!("List {}", display_path(path, cwd)),
        kind: agent_client_protocol::schema::v1::ToolKind::Read,
        content: vec![ux_card("list_directory", "⏳ pending", args, Some((&input, CardBodyKind::Input, false)), None)],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd))],
    }
}

fn search(args: &Value, cwd: &Path) -> ToolInfo {
    let pattern = arg_str(args, "pattern").unwrap_or("");
    let path = arg_str(args, "path").unwrap_or(".");
    let input = if path == "." { format!("pattern `{}`", truncate(pattern, 72)) }
    else { format!("pattern `{}`  ·  path {}", truncate(pattern, 56), display_path(path, cwd)) };
    ToolInfo {
        title: if path == "." { format!("Find `{}`", truncate(pattern, 72)) } else { format!("Find `{}` in {}", truncate(pattern, 56), display_path(path, cwd)) },
        kind: agent_client_protocol::schema::v1::ToolKind::Search,
        content: vec![ux_card("search", "⏳ pending", args, Some((&input, CardBodyKind::Input, false)), None)],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd))],
    }
}

fn search_and_read(args: &Value, cwd: &Path) -> ToolInfo {
    let pattern = arg_str(args, "pattern").unwrap_or("");
    let path = arg_str(args, "path").unwrap_or(".");
    let context = args.get("context").and_then(Value::as_u64).unwrap_or(0);
    let input = if path == "." { format!("pattern `{}`  ·  context ±{}", truncate(pattern, 56), context) }
    else { format!("pattern `{}`  ·  path {}  ·  context ±{}", truncate(pattern, 40), display_path(path, cwd), context) };
    ToolInfo {
        title: if path == "." { format!("Find excerpts for `{}`", truncate(pattern, 56)) } else { format!("Find excerpts for `{}` in {}", truncate(pattern, 40), display_path(path, cwd)) },
        kind: agent_client_protocol::schema::v1::ToolKind::Search,
        content: vec![ux_card("search_and_read", "⏳ pending", args, Some((&input, CardBodyKind::Input, false)), None)],
        locations: vec![ToolCallLocation::new(resolve_path(path, cwd))],
    }
}

fn shell_exec(args: &Value, terminal_id: Option<&str>) -> ToolInfo {
    let command = arg_str(args, "command").unwrap_or("");
    let mut content = vec![ux_card("shell_exec", "⏳ pending", args, Some((command, CardBodyKind::Input, false)), terminal_id)];
    if let Some(id) = terminal_id { content.push(ToolCallContent::Terminal(agent_client_protocol::schema::v1::Terminal::new(id.to_owned()))); }
    ToolInfo {
        title: if command.is_empty() { "Terminal".into() } else { truncate(command, 96) },
        kind: agent_client_protocol::schema::v1::ToolKind::Execute,
        content,
        locations: vec![],
    }
}

fn ask_user_question(args: &Value) -> ToolInfo {
    let title = ask_user_title(args);
    let body = render_ask_user_input(args);
    ToolInfo {
        title,
        kind: agent_client_protocol::schema::v1::ToolKind::Other,
        content: vec![ux_card("AskUserQuestion", "⏳ waiting for user", args, Some((&body, CardBodyKind::Content, false)), None)],
        locations: vec![],
    }
}

fn generic(name: &str, args: &Value) -> ToolInfo {
    let body = if args.as_object().is_none_or(|obj| obj.is_empty()) { "No input payload.".to_owned() } else { concise_args(args) };
    ToolInfo {
        title: name.to_owned(),
        kind: agent_client_protocol::schema::v1::ToolKind::Other,
        content: vec![ux_card(name, "⏳ pending", args, Some((&body, CardBodyKind::Input, false)), None)],
        locations: vec![],
    }
}

fn text_content(text: &str, error: bool) -> ToolCallContent {
    let rendered = if error { format!("⚠️ {text}") } else { text.to_owned() };
    ToolCallContent::Content(Content::new(ContentBlock::Text(TextContent::new(rendered))))
}

fn render_ask_user_input(args: &Value) -> String {
    let Some(questions) = args.get("questions").and_then(Value::as_array) else { return "Question indisponible.".into(); };
    let mut output = String::new();
    for (index, question) in questions.iter().enumerate() {
        if index > 0 { output.push_str("\n\n"); }
        let header = question.get("header").and_then(Value::as_str).unwrap_or("Question");
        let prompt = question.get("question").and_then(Value::as_str).unwrap_or("Question indisponible.");
        output.push_str(&format!("{header}\n{prompt}"));
        if let Some(options) = question.get("options").and_then(Value::as_array) {
            for option in options {
                if let Some(label) = option.get("label").and_then(Value::as_str) { output.push_str(&format!("\n- {label}")); }
            }
        }
    }
    truncate(&output, MAX_QUESTION_PREVIEW_CHARS)
}

fn render_ask_user_result(result: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(result) else { return result.to_owned() };
    let Some(answers) = value.get("answers").and_then(Value::as_object) else { return result.to_owned() };
    if answers.is_empty() { return "Aucune réponse sélectionnée.".into(); }
    answers.iter().map(|(question, answer)| format!("{question}\n{}", answer_display(answer))).collect::<Vec<_>>().join("\n\n")
}

fn answer_display(value: &Value) -> String {
    match value {
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

fn ask_user_title(args: &Value) -> String {
    let question = args.get("questions").and_then(Value::as_array).and_then(|questions| questions.first()).and_then(|question| question.get("question")).and_then(Value::as_str).unwrap_or("User input");
    format!("Ask user · {}", truncate(question, 72))
}

fn file_location(args: &Value, cwd: &Path) -> Vec<ToolCallLocation> {
    arg_str(args, "path").map(|path| vec![ToolCallLocation::new(resolve_path(path, cwd))]).unwrap_or_default()
}

fn filesystem_result_locations(tool_name: &str, result: &str, cwd: &Path) -> Vec<ToolCallLocation> {
    if tool_name == "list_directory" { return vec![]; }
    result.lines().take(MAX_RESULT_LOCATIONS).filter_map(|line| {
        let path = PathBuf::from(line.trim());
        if path.as_os_str().is_empty() { None } else { Some(ToolCallLocation::new(resolve_path(&path.to_string_lossy(), cwd))) }
    }).collect()
}

fn search_result_locations(result: &str, cwd: &Path) -> Vec<ToolCallLocation> {
    let mut locations = Vec::new();
    let mut seen = BTreeSet::new();
    for line in result.lines() {
        let candidate = line.strip_prefix("## ").unwrap_or(line);
        let Some((path, line_number, _)) = split_path_line(candidate) else { continue };
        let resolved = resolve_path(path, cwd);
        let key = format!("{}:{line_number}", resolved.display());
        if seen.insert(key) { locations.push(ToolCallLocation::new(resolved).line(line_number)); }
        if locations.len() >= MAX_RESULT_LOCATIONS { break; }
    }
    locations
}

fn normalize_search_result(tool_name: &str, result: &str, cwd: &Path) -> String {
    let mut output = String::with_capacity(result.len().min(MAX_RESULT_PREVIEW_CHARS));
    for (index, line) in result.lines().enumerate() {
        if output.chars().count() >= MAX_RESULT_PREVIEW_CHARS { break; }
        if index > 0 { output.push('\n'); }
        if tool_name == "search_and_read" && line.starts_with("## ") { output.push_str(&normalize_heading_path(line, cwd)); }
        else { output.push_str(&normalize_match_line(line, cwd)); }
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

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> { args.get(key).and_then(Value::as_str) }

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
        "shell_exec" => arg_str(args, "command").and_then(|command| ShellSandbox::new().analyze_command(command).ok()).map(|ShellAnalysis { risk, .. }| risk).unwrap_or(RiskLevel::Critical),
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

    fn text_of(content: &ToolCallContent) -> String { format!("{content:?}") }

    #[test]
    fn filesystem_tools_have_the_same_card_contract() {
        let cwd = Path::new("/tmp/project");
        for (name, args) in [
            ("glob", serde_json::json!({"pattern":"**/*.rs","path":"src","max_results":20})),
            ("list_directory", serde_json::json!({"path":"src"})),
        ] {
            let info = ToolInfo::build(name, &args, cwd, None);
            assert_eq!(info.content.iter().filter(|item| matches!(item, ToolCallContent::Content(_))).count(), 1);
            let rendered = text_of(&info.content[0]);
            assert!(rendered.contains("Input"));
            assert!(rendered.contains("pending"));
        }
    }

    #[test]
    fn completed_glob_keeps_paths_inside_output_card() {
        let args = serde_json::json!({"pattern":"**/*.rs","path":"src"});
        let update = result_update("glob", &args, "/tmp/project/src/a.rs\n/tmp/project/src/b.rs", true, Path::new("/tmp/project"), None);
        assert_eq!(update.content.len(), 1);
        let rendered = text_of(&update.content[0]);
        assert!(rendered.contains("Glob"));
        assert!(rendered.contains("Output"));
        assert!(rendered.contains("src/a.rs"));
    }

    #[test]
    fn completed_directory_list_keeps_listing_inside_output_card() {
        let args = serde_json::json!({"path":"src"});
        let update = result_update("list_directory", &args, "dir\tutils\nfile\tlib.rs", true, Path::new("/tmp/project"), None);
        assert_eq!(update.content.len(), 1);
        let rendered = text_of(&update.content[0]);
        assert!(rendered.contains("Directory"));
        assert!(rendered.contains("Output"));
        assert!(rendered.contains("file\tlib.rs"));
    }

    #[test]
    fn core_tools_keep_one_text_card() {
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
            assert_eq!(info.content.iter().filter(|item| matches!(item, ToolCallContent::Content(_))).count(), 1, "expected one text card for {name}");
        }
    }

    #[test]
    fn completed_file_read_keeps_numbered_output_inside_card() {
        let args = serde_json::json!({"path":"a.txt","offset":10});
        let update = result_update("file_read", &args, "line a\nline b", true, Path::new("/tmp/project"), None);
        assert_eq!(update.content.len(), 1);
        let rendered = text_of(&update.content[0]);
        assert!(rendered.contains("10\tline a"));
        assert!(rendered.contains("11\tline b"));
        assert!(rendered.contains("Output"));
    }

    #[test]
    fn write_edit_keep_diff_and_shell_keeps_terminal() {
        let cwd = Path::new("/tmp/project");
        for name in ["file_write", "file_edit", "replace_in_file"] {
            let args = serde_json::json!({"path":"src/lib.rs","old_string":"a","new_string":"b","content":"fn main() {}"});
            let info = ToolInfo::build(name, &args, cwd, None);
            assert!(info.content.iter().any(|item| matches!(item, ToolCallContent::Diff(_))));
        }
        let shell = ToolInfo::build("shell_exec", &serde_json::json!({"command":"cargo test"}), cwd, Some("term_1"));
        assert!(shell.content.iter().any(|item| matches!(item, ToolCallContent::Terminal(_))));
    }

    #[test]
    fn ask_user_is_human_readable() {
        let args = serde_json::json!({"questions":[{"header":"Cleanup","question":"Keep the file?","options":[{"label":"Keep"},{"label":"Delete"}]}]});
        let info = ToolInfo::build("AskUserQuestion", &args, Path::new("/tmp"), None);
        let rendered = text_of(&info.content[0]);
        assert!(rendered.contains("Cleanup"));
        assert!(rendered.contains("Keep the file?"));
        assert!(rendered.contains("Keep"));
        assert!(!rendered.contains("\\\"questions\\\""));
    }

    #[test]
    fn lifecycle_and_risk_are_stable() {
        assert_eq!(classify_risk("glob", &serde_json::json!({})), RiskLevel::Low);
        assert_eq!(classify_risk("list_directory", &serde_json::json!({})), RiskLevel::Low);
        assert_eq!(lifecycle_label(ToolLifecycleState::Completed), "completed");
        assert_eq!(lifecycle_icon(ToolLifecycleState::Completed), "🟢");
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
