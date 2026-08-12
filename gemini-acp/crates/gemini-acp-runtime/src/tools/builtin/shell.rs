//! Outil shell : `shell_exec`.
//!
//! Exécute une commande shell dans le CWD de la session.
//! Timeout dur de 30s pour éviter les processus zombies.
//! Capture stdout + stderr (tronqué à 32 KiB).
//!
//! Sécurité : utilise `sandbox::ShellSandbox` pour bloquer les commandes dangereuses.

use std::path::{Path, PathBuf};


use serde_json::{json, Value};

use crate::tools::registry::{Tool, ToolDef, ToolResult};
use crate::tools::sandbox;

const TIMEOUT_SECS: u64 = 30;
const MAX_OUTPUT: usize = 32 * 1024;

fn shell_params() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "Commande shell à exécuter"
            },
            "timeout": {
                "type": "integer",
                "description": "Timeout en secondes (défaut 30, max 120)"
            }
        },
        "required": ["command"]
    })
}

fn shell_def() -> ToolDef {
    ToolDef {
        name: "shell_exec",
        description: "Exécute une commande shell dans le répertoire de travail. \
Retourne le stdout et stderr combinés.",
        parameters_fn: shell_params,
    }
}

pub struct ShellExecTool;

#[async_trait::async_trait]
impl Tool for ShellExecTool {
    fn definition(&self) -> &ToolDef {
        static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
        DEF.get_or_init(shell_def)
    }

    async fn execute(
        &self,
        args: &Value,
        cwd: &Path,
        _allowed_dirs: &[PathBuf],
    ) -> ToolResult {
        let command = match args.get("command").and_then(Value::as_str) {
            Some(c) => c,
            None => return ToolResult::Err("paramètre 'command' manquant".into()),
        };
        let timeout = args
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(TIMEOUT_SECS)
            .min(120);

        // Validation sandbox : bloque les commandes dangereuses.
        // La sandbox est toujours active pour shell (pas de flag pour désactiver
        // depuis le Tool trait — la config est dans ToolRegistry.sandbox).
        let sb = sandbox::ShellSandbox::new();
        if let Err(e) = sb.validate(command) {
            tracing::warn!(command = %command, "commande bloquée par la sandbox");
            return ToolResult::Err(e.to_string());
        }

        tracing::info!(command = %command, cwd = %cwd.display(), timeout = timeout, "shell_exec");

        match tokio::time::timeout(
            std::time::Duration::from_secs(timeout),
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(cwd)
                .output(),
        )
        .await
        {
            Ok(Ok(output)) => {
                let mut result = String::new();
                if !output.stdout.is_empty() {
                    result.push_str(&String::from_utf8_lossy(&output.stdout));
                }
                if !output.stderr.is_empty() {
                    if !result.is_empty() {
                        result.push_str("\n[stderr]\n");
                    }
                    result.push_str(&String::from_utf8_lossy(&output.stderr));
                }
                if result.is_empty() {
                    result = "(sortie vide)".into();
                }
                if result.len() > MAX_OUTPUT {
                    result.truncate(MAX_OUTPUT);
                    result.push_str("\n… (tronqué)");
                }
                let status = if output.status.success() {
                    "exit code 0"
                } else {
                    &format!("exit code {}", output.status.code().unwrap_or(-1))
                };
                ToolResult::Ok(format!("[{status}]\n{result}"))
            }
            Ok(Err(e)) => ToolResult::Err(format!("échec de l'exécution : {e}")),
            Err(_) => ToolResult::Err(format!("timeout après {timeout}s")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shell_echo() {
        let tool = ShellExecTool;
        let args = json!({"command": "echo hello"});
        let result = tool.execute(&args, Path::new("/tmp"), &[]).await;
        match result {
            ToolResult::Ok(s) => {
                assert!(s.contains("hello"));
                assert!(s.contains("exit code 0"));
            }
            ToolResult::Err(e) => panic!("erreur inattendue : {e}"),
        }
    }

    #[tokio::test]
    async fn shell_timeout() {
        let tool = ShellExecTool;
        let args = json!({"command": "sleep 60", "timeout": 1});
        let result = tool.execute(&args, Path::new("/tmp"), &[]).await;
        assert!(matches!(result, ToolResult::Err(e) if e.contains("timeout")));
    }

    #[tokio::test]
    async fn shell_sudo_bloque() {
        let tool = ShellExecTool;
        let args = json!({"command": "sudo rm -rf /"});
        let result = tool.execute(&args, Path::new("/tmp"), &[]).await;
        assert!(matches!(result, ToolResult::Err(e) if e.contains("Sécurité")));
    }

    #[tokio::test]
    async fn shell_shutdown_bloque() {
        let tool = ShellExecTool;
        let args = json!({"command": "shutdown now"});
        let result = tool.execute(&args, Path::new("/tmp"), &[]).await;
        assert!(matches!(result, ToolResult::Err(e) if e.contains("Sécurité")));
    }

    #[tokio::test]
    async fn shell_git_autorise() {
        let tool = ShellExecTool;
        let args = json!({"command": "git status"});
        let result = tool.execute(&args, Path::new("/tmp"), &[]).await;
        // git status peut échouer si pas un repo, mais pas bloqué par sandbox
        assert!(!matches!(result, ToolResult::Err(e) if e.contains("Sécurité")));
    }
}
