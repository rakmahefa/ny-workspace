//! Builtin filesystem tools.
//!
//! The implementation intentionally stays inside the existing `Tool`/`ToolRegistry`
//! architecture.  It follows the same user-facing model as Claude Agent ACP's
//! filesystem tools: read, write, and precise string replacement/editing.
//!
//! Security is centralized through `sandbox::validate_path`; tools never bypass
//! the session sandbox when resolving paths.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::tools::registry::{Tool, ToolDef, ToolResult};
use crate::tools::sandbox;

const DEFAULT_READ_LIMIT: usize = 500;
const MAX_READ_BYTES: u64 = 16 * 1024 * 1024;

fn required_string(args: &Value, name: &str) -> Result<&str, ToolResult> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ToolResult::Err(format!("paramètre '{name}' manquant ou vide")))
}

fn resolve_path(
    args: &Value,
    cwd: &Path,
    allowed_dirs: &[PathBuf],
) -> Result<PathBuf, ToolResult> {
    let raw = required_string(args, "path")?;
    sandbox::validate_path(raw, cwd, allowed_dirs)
        .map_err(|error| ToolResult::Err(error.to_string()))
}

fn file_read_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Chemin du fichier à lire."},
            "offset": {"type": "integer", "minimum": 1, "description": "Première ligne à retourner, indexée à 1. Défaut: 1."},
            "limit": {"type": "integer", "minimum": 1, "description": "Nombre maximal de lignes. Défaut: 500."}
        },
        "required": ["path"]
    })
}

fn file_read_def() -> ToolDef {
    ToolDef {
        name: "file_read",
        description: "Lit un fichier texte avec pagination par lignes. Utilise offset/limit pour les gros fichiers.",
        parameters_fn: file_read_params,
    }
}

pub struct FileReadTool;

#[async_trait::async_trait]
impl Tool for FileReadTool {
    fn definition(&self) -> &ToolDef {
        static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
        DEF.get_or_init(file_read_def)
    }

    async fn execute(&self, args: &Value, cwd: &Path, allowed_dirs: &[PathBuf]) -> ToolResult {
        let path = match resolve_path(args, cwd, allowed_dirs) {
            Ok(path) => path,
            Err(error) => return error,
        };

        let offset = args
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_READ_LIMIT as u64)
            .max(1) as usize;

        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) => metadata,
            Err(error) => {
                return ToolResult::Err(format!("impossible d'accéder à {}: {error}", path.display()))
            }
        };
        if !metadata.is_file() {
            return ToolResult::Err(format!("{} n'est pas un fichier", path.display()));
        }

        if metadata.len() > MAX_READ_BYTES {
            return read_large_file(&path, offset, limit).await;
        }

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(error) => {
                return ToolResult::Err(format!("impossible de lire {}: {error}", path.display()))
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let start = offset.saturating_sub(1).min(lines.len());
        let end = start.saturating_add(limit).min(lines.len());
        let selected = &lines[start..end];

        let mut output = String::new();
        for (index, line) in selected.iter().enumerate() {
            if index > 0 {
                output.push('\n');
            }
            output.push_str(line);
        }

        ToolResult::Ok(output)
    }
}

async fn read_large_file(path: &Path, offset: usize, limit: usize) -> ToolResult {
    use tokio::io::AsyncBufReadExt;

    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(error) => return ToolResult::Err(format!("impossible d'ouvrir {}: {error}", path.display())),
    };

    let mut lines = tokio::io::BufReader::new(file).lines();
    let mut current = 1usize;
    let mut output = String::new();
    let end = offset.saturating_add(limit);

    while current < offset {
        match lines.next_line().await {
            Ok(Some(_)) => current += 1,
            Ok(None) => return ToolResult::Ok(String::new()),
            Err(error) => return ToolResult::Err(format!("erreur de lecture de {}: {error}", path.display())),
        }
    }

    while current < end {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&line);
                current += 1;
            }
            Ok(None) => break,
            Err(error) => return ToolResult::Err(format!("erreur de lecture de {}: {error}", path.display())),
        }
    }

    ToolResult::Ok(output)
}

fn file_write_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Chemin du fichier à écrire."},
            "content": {"type": "string", "description": "Nouveau contenu intégral du fichier."}
        },
        "required": ["path", "content"]
    })
}

fn file_write_def() -> ToolDef {
    ToolDef {
        name: "file_write",
        description: "Écrit intégralement un fichier dans la sandbox, en créant les répertoires parents.",
        parameters_fn: file_write_params,
    }
}

pub struct FileWriteTool;

#[async_trait::async_trait]
impl Tool for FileWriteTool {
    fn definition(&self) -> &ToolDef {
        static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
        DEF.get_or_init(file_write_def)
    }

    async fn execute(&self, args: &Value, cwd: &Path, allowed_dirs: &[PathBuf]) -> ToolResult {
        let path = match resolve_path(args, cwd, allowed_dirs) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let content = match required_string(args, "content") {
            Ok(content) => content,
            Err(error) => return error,
        };

        match write_atomic(&path, content).await {
            Ok(()) => ToolResult::Ok(format!("Fichier écrit: {}", path.display())),
            Err(error) => ToolResult::Err(format!("impossible d'écrire {}: {error}", path.display())),
        }
    }
}

fn file_edit_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Chemin du fichier à modifier."},
            "old_string": {"type": "string", "description": "Texte existant à remplacer."},
            "new_string": {"type": "string", "description": "Nouveau texte."},
            "replace_all": {"type": "boolean", "description": "Remplace toutes les occurrences. Défaut: false."}
        },
        "required": ["path", "old_string", "new_string"]
    })
}

fn file_edit_def() -> ToolDef {
    ToolDef {
        name: "file_edit",
        description: "Modifie précisément un fichier par remplacement de texte; échoue si la cible est absente ou ambiguë.",
        parameters_fn: file_edit_params,
    }
}

pub struct FileEditTool;

#[async_trait::async_trait]
impl Tool for FileEditTool {
    fn definition(&self) -> &ToolDef {
        static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
        DEF.get_or_init(file_edit_def)
    }

    async fn execute(&self, args: &Value, cwd: &Path, allowed_dirs: &[PathBuf]) -> ToolResult {
        let path = match resolve_path(args, cwd, allowed_dirs) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let old = match required_string(args, "old_string") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let new = args.get("new_string").and_then(Value::as_str).unwrap_or("");
        let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);

        let original = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(error) => return ToolResult::Err(format!("impossible de lire {}: {error}", path.display())),
        };

        let occurrences = original.matches(old).count();
        if occurrences == 0 {
            return ToolResult::Err(format!("texte cible introuvable dans {}", path.display()));
        }
        if !replace_all && occurrences != 1 {
            return ToolResult::Err(format!(
                "texte cible ambigu dans {}: {occurrences} occurrences; utilise replace_all=true pour toutes les remplacer",
                path.display()
            ));
        }

        let updated = if replace_all {
            original.replace(old, new)
        } else {
            original.replacen(old, new, 1)
        };

        if let Err(error) = write_atomic(&path, &updated).await {
            return ToolResult::Err(format!("impossible d'écrire {}: {error}", path.display()));
        }

        ToolResult::Ok(format!(
            "Fichier modifié: {} ({} occurrence{})",
            path.display(),
            if replace_all { occurrences } else { 1 },
            if occurrences == 1 { "" } else { "s" }
        ))
    }
}

async fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let temp = path.with_extension(format!("acp-tmp-{}", uuid::Uuid::new_v4().simple()));
    tokio::fs::write(&temp, content).await?;
    if let Err(error) = tokio::fs::rename(&temp, path).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acp-filesystem-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        dir
    }

    #[tokio::test]
    async fn read_with_one_based_offset() {
        let dir = temp_dir().await;
        let path = dir.join("test.txt");
        tokio::fs::write(&path, "one\ntwo\nthree\n").await.unwrap();
        let result = FileReadTool.execute(&json!({"path": "test.txt", "offset": 2, "limit": 1}), &dir, &[]).await;
        assert!(matches!(result, ToolResult::Ok(value) if value == "two"));
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn edit_rejects_ambiguous_match() {
        let dir = temp_dir().await;
        let path = dir.join("test.txt");
        tokio::fs::write(&path, "x\nx\n").await.unwrap();
        let result = FileEditTool.execute(&json!({"path": "test.txt", "old_string": "x", "new_string": "y"}), &dir, &[]).await;
        assert!(matches!(result, ToolResult::Err(error) if error.contains("ambigu")));
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn edit_replace_all_is_atomic_from_callers_perspective() {
        let dir = temp_dir().await;
        let path = dir.join("test.txt");
        tokio::fs::write(&path, "x\nx\n").await.unwrap();
        let result = FileEditTool.execute(&json!({"path": "test.txt", "old_string": "x", "new_string": "y", "replace_all": true}), &dir, &[]).await;
        assert!(result.is_ok());
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "y\ny\n");
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn traversal_is_blocked() {
        let dir = temp_dir().await;
        let result = FileReadTool.execute(&json!({"path": "../../etc/passwd"}), &dir, &[]).await;
        assert!(matches!(result, ToolResult::Err(error) if error.contains("Sécurité")));
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
