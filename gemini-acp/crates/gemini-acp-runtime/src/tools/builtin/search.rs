//! Native search primitives for the builtin tool registry.
//!
//! `search` replaces the previous external `grep` subprocess with Rust-native
//! traversal + regex matching. It supports file globs, bounded results,
//! binary-file skipping, timeouts, and the same sandbox path validation.

use std::path::{Path, PathBuf};
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};

use crate::tools::registry::{Tool, ToolDef, ToolResult};
use crate::tools::sandbox;

const MAX_MATCHES: usize = 200;
const MAX_FILES: usize = 20_000;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

fn search_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Regex de recherche."},
            "path": {"type": "string", "description": "Fichier ou répertoire. Défaut: CWD."},
            "glob": {"type": "string", "description": "Filtre de fichiers, ex: *.rs ou src/**/*.rs."},
            "max_results": {"type": "integer", "minimum": 1, "maximum": 200, "description": "Nombre maximum de correspondances."}
        },
        "required": ["pattern"]
    })
}

fn search_def() -> ToolDef {
    ToolDef {
        name: "search",
        description: "Recherche nativement une regex dans les fichiers texte d'un chemin, avec filtre glob et résultats bornés.",
        parameters_fn: search_params,
    }
}

pub struct SearchTool;

#[async_trait::async_trait]
impl Tool for SearchTool {
    fn definition(&self) -> &ToolDef {
        static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
        DEF.get_or_init(search_def)
    }

    async fn execute(&self, args: &Value, cwd: &Path, allowed_dirs: &[PathBuf]) -> ToolResult {
        let pattern = match args.get("pattern").and_then(Value::as_str) {
            Some(value) if !value.is_empty() => value,
            _ => return ToolResult::Err("paramètre 'pattern' manquant ou vide".into()),
        };
        let regex = match Regex::new(pattern) {
            Ok(regex) => regex,
            Err(error) => return ToolResult::Err(format!("regex invalide: {error}")),
        };
        let root = match args.get("path").and_then(Value::as_str) {
            Some(path) if !path.is_empty() => match sandbox::validate_path(path, cwd, allowed_dirs) {
                Ok(path) => path,
                Err(error) => return ToolResult::Err(error.to_string()),
            },
            _ => cwd.to_path_buf(),
        };
        let glob = args.get("glob").and_then(Value::as_str).filter(|v| !v.is_empty()).map(ToOwned::to_owned);
        let max_results = args.get("max_results").and_then(Value::as_u64).unwrap_or(50).clamp(1, MAX_MATCHES as u64) as usize;

        let future = search_tree(root.clone(), regex, glob, max_results);
        match tokio::time::timeout(SEARCH_TIMEOUT, future).await {
            Ok(Ok(result)) => ToolResult::Ok(result.format()),
            Ok(Err(error)) => ToolResult::Err(error),
            Err(_) => ToolResult::Err(format!("recherche interrompue après {}s", SEARCH_TIMEOUT.as_secs())),
        }
    }
}

#[derive(Debug, Default)]
struct SearchOutput {
    matches: Vec<String>,
    files_scanned: usize,
    truncated: bool,
}

impl SearchOutput {
    fn format(self) -> String {
        if self.matches.is_empty() {
            return format!("Aucune correspondance. ({} fichiers inspectés)", self.files_scanned);
        }
        let mut output = self.matches.join("\n");
        if self.truncated {
            output.push_str("\n… résultats tronqués");
        }
        output.push_str(&format!("\n({} correspondance(s), {} fichiers inspectés)", self.matches.len(), self.files_scanned));
        output
    }
}

async fn search_tree(root: PathBuf, regex: Regex, glob: Option<String>, max_results: usize) -> Result<SearchOutput, String> {
    let metadata = tokio::fs::metadata(&root).await.map_err(|e| format!("chemin introuvable {}: {e}", root.display()))?;
    if metadata.is_file() {
        let mut output = SearchOutput::default();
        search_file(&root, &regex, glob.as_deref(), max_results, &mut output).await?;
        return Ok(output);
    }
    if !metadata.is_dir() {
        return Err(format!("{} n'est ni un fichier ni un répertoire", root.display()));
    }

    let mut output = SearchOutput::default();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir).await.map_err(|e| format!("lecture impossible {}: {e}", dir.display()))?;
        while let Some(entry) = entries.next_entry().await.map_err(|e| format!("lecture impossible {}: {e}", dir.display()))? {
            if output.matches.len() >= max_results {
                output.truncated = true;
                return Ok(output);
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".git" || name == "target" || name == "node_modules" || name == ".venv" || name == "__pycache__" {
                continue;
            }
            let file_type = entry.file_type().await.map_err(|e| format!("type impossible {}: {e}", path.display()))?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                search_file(&path, &regex, glob.as_deref(), max_results, &mut output).await?;
            }
            if output.files_scanned >= MAX_FILES {
                output.truncated = true;
                return Ok(output);
            }
        }
    }
    Ok(output)
}

async fn search_file(path: &Path, regex: &Regex, glob: Option<&str>, max_results: usize, output: &mut SearchOutput) -> Result<(), String> {
    if output.matches.len() >= max_results || output.files_scanned >= MAX_FILES {
        output.truncated = true;
        return Ok(());
    }
    if let Some(glob) = glob {
        if !glob_matches(glob, path) {
            return Ok(());
        }
    }
    let metadata = tokio::fs::metadata(path).await.map_err(|e| format!("stat impossible {}: {e}", path.display()))?;
    if metadata.len() > MAX_FILE_BYTES {
        return Ok(());
    }
    let bytes = tokio::fs::read(path).await.map_err(|e| format!("lecture impossible {}: {e}", path.display()))?;
    output.files_scanned += 1;
    if bytes.contains(&0) {
        return Ok(());
    }
    let text = String::from_utf8_lossy(&bytes);
    for (index, line) in text.lines().enumerate() {
        if regex.is_match(line) {
            output.matches.push(format!("{}:{}:{}", path.display(), index + 1, line));
            if output.matches.len() >= max_results {
                output.truncated = true;
                break;
            }
        }
    }
    Ok(())
}

fn glob_matches(pattern: &str, path: &Path) -> bool {
    let normalized = path.to_string_lossy().replace('\\', "/");
    let pattern = pattern.replace('\\', "/");
    let regex = glob_to_regex(&pattern);
    Regex::new(&regex).map(|re| re.is_match(&normalized) || path.file_name().map(|n| re.is_match(&n.to_string_lossy())).unwrap_or(false)).unwrap_or(false)
}

fn glob_to_regex(pattern: &str) -> String {
    let mut regex = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
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
            '/' => {
                regex.push('/');
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn search_native_basic_and_glob() {
        let dir = std::env::temp_dir().join(format!("acp-search-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(dir.join("src")).await.unwrap();
        tokio::fs::write(dir.join("src/a.rs"), "fn hello() {}\n").await.unwrap();
        tokio::fs::write(dir.join("src/a.txt"), "fn should_not_match\n").await.unwrap();
        let result = SearchTool.execute(&json!({"pattern":"fn hello","path":"src","glob":"*.rs"}), &dir, &[]).await;
        assert!(matches!(result, ToolResult::Ok(value) if value.contains("a.rs:1:fn hello")));
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn invalid_regex_is_reported() {
        let dir = std::env::temp_dir().join(format!("acp-search-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let result = SearchTool.execute(&json!({"pattern":"["}), &dir, &[]).await;
        assert!(matches!(result, ToolResult::Err(error) if error.contains("regex invalide")));
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn binary_file_is_skipped() {
        let dir = std::env::temp_dir().join(format!("acp-search-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("bin.dat"), b"secret\0pattern").await.unwrap();
        let result = SearchTool.execute(&json!({"pattern":"pattern"}), &dir, &[]).await;
        assert!(matches!(result, ToolResult::Ok(value) if value.contains("Aucune correspondance")));
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn traversal_is_blocked() {
        let dir = std::env::temp_dir().join(format!("acp-search-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let result = SearchTool.execute(&json!({"pattern":"root","path":"/etc"}), &dir, &[]).await;
        assert!(matches!(result, ToolResult::Err(error) if error.contains("Sécurité")));
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
