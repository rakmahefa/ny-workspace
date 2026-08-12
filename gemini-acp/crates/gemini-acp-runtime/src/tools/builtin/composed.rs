//! Outils composés construits au-dessus des primitives builtin.
//!
//! Phase 4 garde une seule architecture : les outils composés réutilisent
//! directement `FileReadTool`, `FileEditTool` et `SearchTool` au lieu de
//! dupliquer leur logique de sandbox ou d'accès disque.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::{file::{FileReadTool, FileEditTool}, search::SearchTool};
use crate::tools::registry::{Tool, ToolDef, ToolResult};

fn search_and_read_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Regex à rechercher."},
            "path": {"type": "string", "description": "Fichier ou répertoire de recherche. Défaut: CWD."},
            "glob": {"type": "string", "description": "Filtre de fichiers, ex: *.rs."},
            "context": {"type": "integer", "minimum": 0, "maximum": 20, "description": "Lignes de contexte autour de chaque match."},
            "max_matches": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Nombre maximum de résultats à inspecter."}
        },
        "required": ["pattern"]
    })
}

fn search_and_read_def() -> ToolDef {
    ToolDef {
        name: "search_and_read",
        description: "Recherche un motif puis lit les extraits correspondants; combine recherche et lecture sans dupliquer les primitives.",
        parameters_fn: search_and_read_params,
    }
}

pub struct SearchAndReadTool;

#[async_trait::async_trait]
impl Tool for SearchAndReadTool {
    fn definition(&self) -> &ToolDef {
        static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
        DEF.get_or_init(search_and_read_def)
    }

    async fn execute(&self, args: &Value, cwd: &Path, allowed_dirs: &[PathBuf]) -> ToolResult {
        let pattern = match args.get("pattern").and_then(Value::as_str) {
            Some(value) if !value.is_empty() => value,
            _ => return ToolResult::Err("paramètre 'pattern' manquant ou vide".into()),
        };
        let max_matches = args.get("max_matches").and_then(Value::as_u64).unwrap_or(20).clamp(1, 100) as usize;
        let context = args.get("context").and_then(Value::as_u64).unwrap_or(2).min(20) as usize;

        let search_args = json!({
            "pattern": pattern,
            "path": args.get("path").and_then(Value::as_str).unwrap_or(""),
            "glob": args.get("glob").and_then(Value::as_str).unwrap_or("")
        });
        let matches = match SearchTool.execute(&search_args, cwd, allowed_dirs).await {
            ToolResult::Ok(value) => value,
            ToolResult::Err(error) => return ToolResult::Err(error),
        };

        let mut result = String::new();
        let mut inspected = 0usize;
        for line in matches.lines() {
            if line.starts_with('(') || line.starts_with('…') || line.trim().is_empty() { continue; }
            let Some((file, rest)) = line.split_once(':') else { continue };
            let Ok(line_no) = rest.split(':').next().unwrap_or("").parse::<usize>() else { continue };
            if inspected >= max_matches { break; }
            let start = line_no.saturating_sub(context).max(1);
            let read_args = json!({"path": file, "offset": start, "limit": context * 2 + 1});
            let excerpt = match FileReadTool.execute(&read_args, cwd, allowed_dirs).await {
                ToolResult::Ok(value) => value,
                ToolResult::Err(error) => format!("[lecture impossible: {error}]"),
            };
            result.push_str(&format!("\n## {file}:{line_no}\n{excerpt}\n"));
            inspected += 1;
        }
        if result.is_empty() { ToolResult::Ok("Aucun extrait correspondant.".into()) } else { ToolResult::Ok(result.trim_start().to_string()) }
    }
}

fn replace_in_file_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "Fichier à modifier."},
            "old_string": {"type": "string", "description": "Texte existant à remplacer."},
            "new_string": {"type": "string", "description": "Nouveau texte."},
            "replace_all": {"type": "boolean", "description": "Remplacer toutes les occurrences."}
        },
        "required": ["path", "old_string", "new_string"]
    })
}

fn replace_in_file_def() -> ToolDef {
    ToolDef {
        name: "replace_in_file",
        description: "Effectue une édition précise via la primitive file_edit; échoue sur une cible absente ou ambiguë.",
        parameters_fn: replace_in_file_params,
    }
}

pub struct ReplaceInFileTool;

#[async_trait::async_trait]
impl Tool for ReplaceInFileTool {
    fn definition(&self) -> &ToolDef {
        static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
        DEF.get_or_init(replace_in_file_def)
    }
    async fn execute(&self, args: &Value, cwd: &Path, allowed_dirs: &[PathBuf]) -> ToolResult {
        FileEditTool.execute(args, cwd, allowed_dirs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn replace_delegates_to_file_edit() {
        let dir = std::env::temp_dir().join(format!("acp-composed-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("test.txt");
        tokio::fs::write(&path, "hello world").await.unwrap();
        let result = ReplaceInFileTool.execute(&json!({"path":"test.txt","old_string":"world","new_string":"rust"}), &dir, &[]).await;
        assert!(result.is_ok());
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), "hello rust");
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
