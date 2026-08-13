//! Native filesystem discovery builtins: `glob` and `list_directory`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::tools::{registry::{Tool, ToolDef, ToolResult}, sandbox};

const MAX_RESULTS: usize = 500;
const MAX_ENTRIES: usize = 2_000;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct GlobTool;
pub struct ListDirectoryTool;

fn glob_def() -> ToolDef {
    ToolDef {
        name: "glob",
        description: "Find filesystem paths matching a glob pattern within the allowed workspace.",
        parameters_fn: || json!({
            "type": "object",
            "properties": {
                "pattern": {"type":"string","description":"Glob pattern such as **/*.rs or src/*.toml."},
                "path": {"type":"string","description":"Directory root. Defaults to the current working directory."},
                "max_results": {"type":"integer","minimum":1,"maximum":500}
            },
            "required": ["pattern"]
        }),
    }
}

fn list_directory_def() -> ToolDef {
    ToolDef {
        name: "list_directory",
        description: "List the direct children of a directory with stable, bounded output.",
        parameters_fn: || json!({
            "type": "object",
            "properties": {
                "path": {"type":"string","description":"Directory to inspect. Defaults to the current working directory."}
            }
        }),
    }
}

#[async_trait::async_trait]
impl Tool for GlobTool {
    fn definition(&self) -> &ToolDef {
        static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
        DEF.get_or_init(glob_def)
    }

    async fn execute(&self, args: &Value, cwd: &Path, allowed_dirs: &[PathBuf]) -> ToolResult {
        let pattern = match args.get("pattern").and_then(Value::as_str) {
            Some(pattern) if !pattern.trim().is_empty() => pattern,
            _ => return ToolResult::Err("paramètre 'pattern' manquant ou vide".into()),
        };
        let root = match args.get("path").and_then(Value::as_str).filter(|v| !v.is_empty()) {
            Some(path) => match sandbox::validate_path(path, cwd, allowed_dirs) {
                Ok(path) => path,
                Err(error) => return ToolResult::Err(error.to_string()),
            },
            None => cwd.to_path_buf(),
        };
        let max_results = args.get("max_results").and_then(Value::as_u64).unwrap_or(100).clamp(1, MAX_RESULTS as u64) as usize;
        let pattern = pattern.replace('\\', "/");

        match tokio::time::timeout(DISCOVERY_TIMEOUT, collect_glob(root, pattern, max_results)).await {
            Ok(Ok(paths)) => {
                if paths.is_empty() { ToolResult::Ok("Aucun chemin correspondant.".into()) }
                else { ToolResult::Ok(format_paths(paths)) }
            }
            Ok(Err(error)) => ToolResult::Err(error),
            Err(_) => ToolResult::Err(format!("glob interrompu après {}s", DISCOVERY_TIMEOUT.as_secs())),
        }
    }
}

#[async_trait::async_trait]
impl Tool for ListDirectoryTool {
    fn definition(&self) -> &ToolDef {
        static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
        DEF.get_or_init(list_directory_def)
    }

    async fn execute(&self, args: &Value, cwd: &Path, allowed_dirs: &[PathBuf]) -> ToolResult {
        let root = match args.get("path").and_then(Value::as_str).filter(|v| !v.is_empty()) {
            Some(path) => match sandbox::validate_path(path, cwd, allowed_dirs) {
                Ok(path) => path,
                Err(error) => return ToolResult::Err(error.to_string()),
            },
            None => cwd.to_path_buf(),
        };

        match tokio::time::timeout(DISCOVERY_TIMEOUT, list_directory(root)).await {
            Ok(Ok(output)) => ToolResult::Ok(output),
            Ok(Err(error)) => ToolResult::Err(error),
            Err(_) => ToolResult::Err(format!("list_directory interrompu après {}s", DISCOVERY_TIMEOUT.as_secs())),
        }
    }
}

async fn collect_glob(root: PathBuf, pattern: String, max_results: usize) -> Result<Vec<PathBuf>, String> {
    let metadata = tokio::fs::metadata(&root).await.map_err(|e| format!("chemin introuvable {}: {e}", root.display()))?;
    if !metadata.is_dir() {
        return Err(format!("{} n'est pas un répertoire", root.display()));
    }

    let mut stack = vec![root.clone()];
    let mut matches = Vec::new();
    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await.map_err(|e| format!("lecture impossible {}: {e}", dir.display()))?;
        while let Some(entry) = entries.next_entry().await.map_err(|e| format!("lecture impossible {}: {e}", dir.display()))? {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if is_ignored_dir(&name) { continue; }
            let file_type = entry.file_type().await.map_err(|e| format!("type impossible {}: {e}", path.display()))?;
            let relative = path.strip_prefix(&root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            if glob_matches(&pattern, &relative, &name) {
                matches.push(path.clone());
                if matches.len() >= max_results {
                    return Ok(matches);
                }
            }
            if file_type.is_dir() && stack.len() < MAX_ENTRIES {
                stack.push(path);
            }
            if matches.len() >= MAX_RESULTS { break; }
        }
    }
    matches.sort();
    Ok(matches)
}

async fn list_directory(root: PathBuf) -> Result<String, String> {
    let metadata = tokio::fs::metadata(&root).await.map_err(|e| format!("chemin introuvable {}: {e}", root.display()))?;
    if !metadata.is_dir() {
        return Err(format!("{} n'est pas un répertoire", root.display()));
    }

    let mut entries = Vec::new();
    let mut dir = tokio::fs::read_dir(&root).await.map_err(|e| format!("lecture impossible {}: {e}", root.display()))?;
    while let Some(entry) = dir.next_entry().await.map_err(|e| format!("lecture impossible {}: {e}", root.display()))? {
        let file_type = entry.file_type().await.map_err(|e| format!("type impossible {}: {e}", entry.path().display()))?;
        let kind = if file_type.is_dir() { "dir" } else if file_type.is_file() { "file" } else { "other" };
        entries.push(format!("{kind}\t{}", entry.file_name().to_string_lossy()));
        if entries.len() >= MAX_ENTRIES { break; }
    }
    entries.sort();
    if entries.is_empty() { return Ok("Répertoire vide.".into()); }
    let truncated = entries.len() >= MAX_ENTRIES;
    let mut output = entries.join("\n");
    if truncated { output.push_str("\n… résultats tronqués"); }
    Ok(output)
}

fn is_ignored_dir(name: &str) -> bool {
    matches!(name, ".git" | "target" | "node_modules" | ".venv" | "__pycache__")
}

fn glob_matches(pattern: &str, relative: &str, basename: &str) -> bool {
    let regex = glob_to_regex(pattern);
    regex::Regex::new(&regex)
        .map(|re| re.is_match(relative) || re.is_match(basename))
        .unwrap_or(false)
}

fn glob_to_regex(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut regex = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                regex.push_str(".*");
                i += 2;
            }
            '*' => {
                regex.push_str("[^/]*");
                i += 1;
            }
            '?' => {
                regex.push_str("[^/]");
                i += 1;
            }
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                regex.push('\\');
                regex.push(chars[i]);
                i += 1;
            }
            c => {
                regex.push(c);
                i += 1;
            }
        }
    }
    regex.push('$');
    regex
}

fn format_paths(paths: Vec<PathBuf>) -> String {
    paths.into_iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn glob_finds_matching_files() {
        let dir = std::env::temp_dir().join(format!("acp-glob-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(dir.join("src")).await.unwrap();
        tokio::fs::write(dir.join("src/lib.rs"), "pub fn x() {}\n").await.unwrap();
        tokio::fs::write(dir.join("src/lib.txt"), "x\n").await.unwrap();
        let result = GlobTool.execute(&json!({"pattern":"**/*.rs"}), &dir, &[]).await;
        assert!(matches!(result, ToolResult::Ok(value) if value.contains("lib.rs")));
        assert!(!matches!(result, ToolResult::Ok(value) if value.contains("lib.txt")));
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn list_directory_is_stable() {
        let dir = std::env::temp_dir().join(format!("acp-list-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("b.txt"), "b").await.unwrap();
        tokio::fs::create_dir(dir.join("a-dir")).await.unwrap();
        let result = ListDirectoryTool.execute(&json!({}), &dir, &[]).await;
        assert!(matches!(result, ToolResult::Ok(value) if value.starts_with("dir\ta-dir\nfile\tb.txt")));
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn traversal_is_blocked() {
        let dir = std::env::temp_dir().join(format!("acp-fs-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let result = GlobTool.execute(&json!({"pattern":"*","path":"/etc"}), &dir, &[]).await;
        assert!(matches!(result, ToolResult::Err(error) if error.contains("Sécurité")));
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
