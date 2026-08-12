//! Outils filesystem : `file_read` et `file_write`.
//!
//! - `file_read` : lit un fichier (supporte les offsets pour les gros fichiers).
//! - `file_write` : écrit intégralement un fichier (crée les répertoires parents si besoin).
//!
//! Sécurité : utilise `sandbox::validate_path` pour bloquer les path traversals.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::tools::registry::{Tool, ToolDef, ToolResult};
use crate::tools::sandbox;

// --- file_read ---

fn file_read_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Chemin du fichier à lire (absolu ou relatif au CWD)"
            },
            "offset": {
                "type": "integer",
                "description": "Ligne de départ (0-indexed). Défaut : 0."
            },
            "limit": {
                "type": "integer",
                "description": "Nombre maximum de lignes à lire. Défaut : 500."
            }
        },
        "required": ["path"]
    })
}

fn file_read_def() -> ToolDef {
    ToolDef {
        name: "file_read",
        description: "Lit le contenu d'un fichier texte. Pour les fichiers volumineux, \
utilise offset et limit pour lire une plage spécifique.",
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
        let raw_path = match args.get("path").and_then(Value::as_str) {
            Some(p) => p,
            None => return ToolResult::Err("paramètre 'path' manquant".into()),
        };

        // Validation de sécurité : anti-traversal.
        let path = match sandbox::validate_path(raw_path, cwd, allowed_dirs) {
            Ok(p) => p,
            Err(e) => return ToolResult::Err(e.to_string()),
        };

        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(500) as usize;

        const MAX_FULL_READ: u64 = 10 * 1024 * 1024;
        let file_meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(e) => {
                return ToolResult::Err(format!("impossible d'accéder à {} : {e}", path.display()));
            }
        };
        let use_line_reader = file_meta.len() > MAX_FULL_READ || offset > 0 || limit < 500;
        let (selected, total_lines) = if use_line_reader {
            let file = match tokio::fs::File::open(&path).await {
                Ok(f) => f,
                Err(e) => {
                    return ToolResult::Err(format!(
                        "impossible d'ouvrir {} : {e}",
                        path.display()
                    ));
                }
            };
            let reader = tokio::io::BufReader::new(file);
            let mut lines_buf = tokio::io::AsyncBufReadExt::lines(reader);
            let mut all_lines = Vec::new();
            while let Ok(Some(line)) = lines_buf.next_line().await {
                all_lines.push(line);
            }
            let total = all_lines.len();
            let sel: Vec<String> = all_lines.into_iter().skip(offset).take(limit).collect();
            (sel, total)
        } else {
            let content = match tokio::fs::read_to_string(&path).await {
                Ok(c) => c,
                Err(e) => {
                    return ToolResult::Err(format!("impossible de lire {} : {e}", path.display()));
                }
            };
            let lines: Vec<&str> = content.lines().collect();
            let total_lines = lines.len();
            let sel: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
            (sel, total_lines)
        };
        let shown = if selected.len() == total_lines {
            total_lines.to_string()
        } else {
            format!(
                "{} (lignes {offset}..{})",
                selected.len(),
                offset + selected.len()
            )
        };
        let result = format!(
            "[{} lignes affichées sur {}]\n{}",
            shown,
            total_lines,
            selected.join("\n")
        );
        ToolResult::Ok(result)
    }
}

// --- file_write ---

fn file_write_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Chemin du fichier à écrire (absolu ou relatif au CWD)"
            },
            "content": {
                "type": "string",
                "description": "Contenu intégral à écrire dans le fichier"
            }
        },
        "required": ["path", "content"]
    })
}

fn file_write_def() -> ToolDef {
    ToolDef {
        name: "file_write",
        description: "Écrit le contenu intégral dans un fichier. Crée les répertoires \
parents si nécessaire.",
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
        let raw_path = match args.get("path").and_then(Value::as_str) {
            Some(p) => p,
            None => return ToolResult::Err("paramètre 'path' manquant".into()),
        };
        let content = match args.get("content").and_then(Value::as_str) {
            Some(c) => c,
            None => return ToolResult::Err("paramètre 'content' manquant".into()),
        };

        // Validation de sécurité : anti-traversal.
        let path = match sandbox::validate_path(raw_path, cwd, allowed_dirs) {
            Ok(p) => p,
            Err(e) => return ToolResult::Err(e.to_string()),
        };

        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolResult::Err(format!(
                    "impossible de créer les répertoires parents pour {} : {e}",
                    path.display()
                ));
            }
        }

        match tokio::fs::write(&path, content).await {
            Ok(()) => ToolResult::Ok(format!(
                "Fichier écrit : {} ({} octets, {} lignes)",
                path.display(),
                content.len(),
                content.lines().count()
            )),
            Err(e) => ToolResult::Err(format!("impossible d'écrire {} : {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_read_basic() {
        let dir =
            std::env::temp_dir().join(format!("acp-tool-test-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let f = dir.join("test.txt");
        tokio::fs::write(&f, "ligne 1\nligne 2\nligne 3")
            .await
            .unwrap();

        let tool = FileReadTool;
        let args = json!({"path": "test.txt"});
        let result = tool.execute(&args, &dir, &[]).await;
        match result {
            ToolResult::Ok(s) => {
                assert!(s.contains("ligne 1"));
                assert!(s.contains("ligne 3"));
            }
            ToolResult::Err(e) => panic!("erreur inattendue : {e}"),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn file_write_basic() {
        let dir =
            std::env::temp_dir().join(format!("acp-tool-test-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool;
        let args = json!({
            "path": "sub/out.txt",
            "content": "hello\nworld"
        });
        let result = tool.execute(&args, &dir, &[]).await;
        match result {
            ToolResult::Ok(s) => {
                assert!(s.contains("écrit"));
            }
            ToolResult::Err(e) => panic!("erreur inattendue : {e}"),
        }
        let target = dir.join("sub").join("out.txt");
        assert_eq!(
            tokio::fs::read_to_string(&target).await.unwrap(),
            "hello\nworld"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn file_read_traversal_bloque() {
        let dir =
            std::env::temp_dir().join(format!("acp-tool-test-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileReadTool;
        let args = json!({"path": "../../etc/passwd"});
        let result = tool.execute(&args, &dir, &[]).await;
        assert!(matches!(result, ToolResult::Err(e) if e.contains("Sécurité")));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn file_write_traversal_bloque() {
        let dir =
            std::env::temp_dir().join(format!("acp-tool-test-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool;
        let args = json!({"path": "/etc/corrupted", "content": "pwned"});
        let result = tool.execute(&args, &dir, &[]).await;
        assert!(matches!(result, ToolResult::Err(e) if e.contains("Sécurité")));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn file_read_allowed_dir() {
        let dir =
            std::env::temp_dir().join(format!("acp-tool-test-{}", uuid::Uuid::new_v4().simple()));
        let other =
            std::env::temp_dir().join(format!("acp-other-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::create_dir_all(&other).await.unwrap();
        tokio::fs::write(other.join("data.txt"), "secret data")
            .await
            .unwrap();

        let tool = FileReadTool;
        let args = json!({"path": other.join("data.txt").to_str().unwrap()});
        let result = tool
            .execute(&args, &dir, std::slice::from_ref(&other))
            .await;
        assert!(matches!(result, ToolResult::Ok(s) if s.contains("secret data")));

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }
}
