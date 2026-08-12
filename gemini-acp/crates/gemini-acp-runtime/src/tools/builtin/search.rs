//! Outil search : `search`.
//!
//! Recherche de motifs (regex) dans les fichiers d'un répertoire.
//! Utilise `grep -rn` pour la recherche rapide.
//! Tronque les résultats à 100 correspondances.
//!
//! Sécurité : valide le chemin de recherche via `sandbox::validate_path`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::tools::registry::{Tool, ToolDef, ToolResult};
use crate::tools::sandbox;

const MAX_MATCHES: usize = 100;
/// Délai maximum d'exécution de `grep` (B16). Un motif ReDoS fourni par le
/// modèle ou un répertoire massif pourrait autrement bloquer l'agent
/// indéfiniment. On couple `--max-count` (par fichier) avec un timeout global.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

fn search_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Motif de recherche (regex étendue)"
            },
            "path": {
                "type": "string",
                "description": "Répertoire ou fichier à chercher (défaut : CWD)"
            },
            "glob": {
                "type": "string",
                "description": "Filtre de fichiers (ex: *.rs). Défaut : tous les fichiers texte"
            }
        },
        "required": ["pattern"]
    })
}

fn search_def() -> ToolDef {
    ToolDef {
        name: "search",
        description: "Recherche un motif (regex) dans les fichiers texte d'un répertoire. \
Retourne les lignes correspondantes avec le chemin et le numéro de ligne.",
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
            Some(p) => p,
            None => return ToolResult::Err("paramètre 'pattern' manquant".into()),
        };

        let search_path = if let Some(p) = args.get("path").and_then(Value::as_str) {
            // Validation de sécurité : anti-traversal.
            match sandbox::validate_path(p, cwd, allowed_dirs) {
                Ok(resolved) => resolved,
                Err(e) => return ToolResult::Err(e.to_string()),
            }
        } else {
            cwd.to_path_buf()
        };

        let mut cmd = tokio::process::Command::new("grep");
        cmd.arg("-rn")
            .arg("--max-count=1")
            .arg("-E")
            .arg("-e")
            .arg(pattern)
            .arg("--")
            .arg(&search_path)
            .arg("--exclude-dir=.git")
            .arg("--exclude-dir=target")
            .arg("--exclude-dir=node_modules");

        if let Some(glob) = args.get("glob").and_then(Value::as_str) {
            cmd.arg("--include").arg(glob);
        }

        // Timeout global (B16) : évite qu'une regex ReDoS ou un arbre massif
        // ne bloque l'agent. On kill le subprocess si le délai est dépassé.
        let output = match tokio::time::timeout(SEARCH_TIMEOUT, cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => {
                return ToolResult::Err(format!("échec de la recherche : {e} (grep installé ?)"));
            }
            Err(_) => {
                return ToolResult::Err(format!(
                    "recherche interrompue : délai de {} s dépassé (motif ReDoS ou trop de fichiers ?)",
                    SEARCH_TIMEOUT.as_secs()
                ));
            }
        };

        if !output.status.success() && output.stdout.is_empty() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("No such file") {
                return ToolResult::Err(format!("chemin introuvable : {}", search_path.display()));
            }
            if stderr.is_empty() {
                return ToolResult::Ok("Aucune correspondance trouvée.".into());
            }
            return ToolResult::Err(format!("erreur grep : {stderr}"));
        }

        let raw = String::from_utf8_lossy(&output.stdout);
        // On prend les MAX_MATCHES premières lignes, puis on ne compte le
        // total QUE si on a atteint la limite (évite de parcourir tout le
        // buffer dans le cas courant où il y a peu de correspondances).
        let lines: Vec<&str> = raw.lines().take(MAX_MATCHES).collect();
        let truncated = lines.len() == MAX_MATCHES;
        let total = if truncated {
            // On doit bien parcourir le reste pour le compte exact.
            raw.lines().count()
        } else {
            lines.len()
        };
        let mut result = lines.join("\n");
        if truncated {
            result.push_str(&format!(
                "\n… ({total} correspondances, {MAX_MATCHES} affichées)"
            ));
        } else {
            result.push_str(&format!("\n({total} correspondance(s))"));
        }
        ToolResult::Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn search_basic() {
        let dir =
            std::env::temp_dir().join(format!("acp-search-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a.rs"), "fn hello() {\n  println!();\n}")
            .await
            .unwrap();

        let tool = SearchTool;
        let args = json!({"pattern": "fn "});
        let result = tool.execute(&args, &dir, &[]).await;
        match result {
            ToolResult::Ok(s) => assert!(s.contains("fn hello")),
            ToolResult::Err(e) => panic!("erreur : {e}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn search_traversal_bloque() {
        let dir =
            std::env::temp_dir().join(format!("acp-search-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = SearchTool;
        let args = json!({"pattern": "root", "path": "/etc"});
        let result = tool.execute(&args, &dir, &[]).await;
        assert!(matches!(result, ToolResult::Err(e) if e.contains("Sécurité")));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn search_allowed_dir() {
        let dir =
            std::env::temp_dir().join(format!("acp-search-{}", uuid::Uuid::new_v4().simple()));
        let other =
            std::env::temp_dir().join(format!("acp-other-{}", uuid::Uuid::new_v4().simple()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::create_dir_all(&other).await.unwrap();
        tokio::fs::write(other.join("data.txt"), "pattern_match")
            .await
            .unwrap();

        let tool = SearchTool;
        let args = json!({
            "pattern": "pattern",
            "path": other.to_str().unwrap()
        });
        let result = tool
            .execute(&args, &dir, std::slice::from_ref(&other))
            .await;
        assert!(matches!(result, ToolResult::Ok(s) if s.contains("pattern_match")));

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other).ok();
    }
}
