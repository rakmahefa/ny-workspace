//! Registre d'outils : définition, dispatch, résultats.
//!
//! Responsabilités :
//! - `ToolDef` : descripteur JSON d'un outil (name, description, parameters).
//! - `ToolResult` : résultat d'exécution (succès ou erreur).
//! - `Tool` trait : interface que chaque outil implémente.
//! - `ToolRegistry` : collection nommée d'outils, lookup + liste des descripteurs.
//!
//! Améliorations (post-extraction) :
//! - `call_async` : dispatch nativement async (plus de `block_on`).
//! - `call` : conservé pour rétrocompatibilité, wrap `call_async` via `block_on`.
//! - `SandboxConfig` : configuration de sandbox transmise aux outils.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// Configuration de sandbox partagée par tous les outils d'une session.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Répertoires autorisés en plus du CWD.
    pub allowed_dirs: Vec<PathBuf>,
    /// Si `true`, la sandbox shell est active (bloque les commandes dangereuses).
    #[allow(dead_code)]
    pub shell_sandbox_enabled: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self { allowed_dirs: Vec::new(), shell_sandbox_enabled: true }
    }
}

/// Descripteur JSON d'un outil, tel qu'injecté dans le prompt Gemini.
/// Format compatible avec la section `# Tool Use` du web2api.
#[derive(Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    /// Schéma JSON des paramètres (objet OpenAPI 3.0 simplifié).
    /// Utilise une closure pour éviter le `static` non-const de `json!()`.
    pub parameters_fn: fn() -> Value,
}

impl std::fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDef").field("name", &self.name).field("description", &self.description).finish()
    }
}

impl ToolDef {
    /// Sérialise en objet JSON `{name, description, parameters}`.
    pub fn to_json(&self) -> Value {
        serde_json::json!({ "name": self.name, "description": self.description, "parameters": (self.parameters_fn)() })
    }
}

/// Résultat d'exécution d'un outil.
#[derive(Debug, Clone)]
pub enum ToolResult {
    Ok(String),
    Err(String),
}

impl ToolResult {
    pub fn is_ok(&self) -> bool { matches!(self, ToolResult::Ok(_)) }

    /// Formate pour injection dans l'historique `[Tool result for <name>]: …`.
    pub fn to_history_text(&self) -> String {
        match self { ToolResult::Ok(s) => s.clone(), ToolResult::Err(e) => format!("[Erreur] {e}") }
    }
}

/// Interface que chaque outil builtin doit implémenter.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> &ToolDef;
    async fn execute(&self, args: &Value, cwd: &Path, allowed_dirs: &[PathBuf]) -> ToolResult;
}

/// Registre : collection nommée d'outils.
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    sandbox: SandboxConfig,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.tools.iter().map(|t| t.definition().name).collect();
        f.debug_struct("ToolRegistry").field("tools", &names).field("sandbox", &self.sandbox).finish()
    }
}

impl Default for ToolRegistry { fn default() -> Self { Self::new() } }

impl ToolRegistry {
    pub fn new() -> Self { Self { tools: Vec::new(), sandbox: SandboxConfig::default() } }

    #[allow(dead_code)]
    pub fn with_sandbox(sandbox: SandboxConfig) -> Self { Self { tools: Vec::new(), sandbox } }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        tracing::debug!(name = tool.definition().name, "outil enregistré");
        self.tools.push(tool);
    }

    /// Construit un registre avec tous les outils builtin.
    pub fn builtin() -> Self {
        let mut reg = Self::new();
        reg.register(Box::new(crate::tools::builtin::file::FileReadTool));
        reg.register(Box::new(crate::tools::builtin::file::FileWriteTool));
        reg.register(Box::new(crate::tools::builtin::file::FileEditTool));
        reg.register(Box::new(crate::tools::builtin::shell::ShellExecTool));
        reg.register(Box::new(crate::tools::builtin::search::SearchTool));
        reg
    }

    /// Construit un registre builtin avec sandbox personnalisée.
    #[allow(dead_code)]
    pub fn builtin_with_sandbox(sandbox: SandboxConfig) -> Self {
        let mut reg = Self::with_sandbox(sandbox);
        reg.register(Box::new(crate::tools::builtin::file::FileReadTool));
        reg.register(Box::new(crate::tools::builtin::file::FileWriteTool));
        reg.register(Box::new(crate::tools::builtin::file::FileEditTool));
        reg.register(Box::new(crate::tools::builtin::shell::ShellExecTool));
        reg.register(Box::new(crate::tools::builtin::search::SearchTool));
        reg
    }

    pub fn definitions(&self) -> Vec<Value> { self.tools.iter().map(|t| t.definition().to_json()).collect() }

    #[allow(dead_code)]
    pub fn sandbox(&self) -> &SandboxConfig { &self.sandbox }

    /// Cherche un outil par nom et l'exécute de façon async native.
    pub async fn call_async(&self, name: &str, args: &Value, cwd: &Path, extra_dirs: &[PathBuf]) -> Option<ToolResult> {
        let tool = self.tools.iter().find(|t| t.definition().name == name)?;
        let mut allowed = self.sandbox.allowed_dirs.clone();
        for dir in extra_dirs {
            if !allowed.contains(dir) { allowed.push(dir.clone()); }
        }
        Some(tool.execute(args, cwd, &allowed).await)
    }

    pub fn has_tools(&self) -> bool { !self.tools.is_empty() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoTool;
    fn echo_def() -> ToolDef {
        ToolDef { name: "echo", description: "Répète le message.", parameters_fn: || json!({
            "type": "object", "properties": { "message": { "type": "string", "description": "Message à répéter" } }, "required": ["message"]
        }) }
    }

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn definition(&self) -> &ToolDef { static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new(); DEF.get_or_init(echo_def) }
        async fn execute(&self, args: &Value, _cwd: &Path, _allowed_dirs: &[PathBuf]) -> ToolResult {
            ToolResult::Ok(args.get("message").and_then(Value::as_str).unwrap_or("").to_string())
        }
    }

    #[test]
    fn registry_builtin_has_tools() {
        let reg = ToolRegistry::builtin();
        assert!(reg.has_tools());
        let defs = reg.definitions();
        assert!(defs.len() >= 5);
        let names: Vec<&str> = defs.iter().filter_map(|d| d.get("name").and_then(Value::as_str)).collect();
        assert!(names.contains(&"file_read"));
        assert!(names.contains(&"file_write"));
        assert!(names.contains(&"file_edit"));
        assert!(names.contains(&"shell_exec"));
        assert!(names.contains(&"search"));
    }

    #[tokio::test]
    async fn call_async_basic() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(EchoTool));
        let result = reg.call_async("echo", &json!({"message": "bonjour"}), Path::new("/tmp"), &[]).await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.to_history_text(), "bonjour");
    }

    #[tokio::test]
    async fn call_async_unknown_tool() {
        let reg = ToolRegistry::new();
        let result = reg.call_async("nonexistent", &json!({}), Path::new("/tmp"), &[]).await;
        assert!(result.is_none());
    }

    #[test]
    fn tool_result_to_history() {
        assert_eq!(ToolResult::Ok("contenu du fichier".into()).to_history_text(), "contenu du fichier");
        assert!(ToolResult::Err("fichier introuvable".into()).to_history_text().contains("[Erreur]"));
    }

    #[test]
    fn tool_def_to_json() {
        let def = ToolDef { name: "test", description: "un test", parameters_fn: || json!({"type": "object"}) };
        let j = def.to_json();
        assert_eq!(j["name"], "test");
        assert_eq!(j["description"], "un test");
    }

    #[test]
    fn sandbox_config_default() {
        let cfg = SandboxConfig::default();
        assert!(cfg.allowed_dirs.is_empty());
        assert!(cfg.shell_sandbox_enabled);
    }

    #[test]
    fn registry_with_custom_sandbox() {
        let cfg = SandboxConfig { allowed_dirs: vec![PathBuf::from("/opt/project")], shell_sandbox_enabled: false };
        let reg = ToolRegistry::builtin_with_sandbox(cfg);
        assert!(!reg.sandbox().shell_sandbox_enabled);
        assert_eq!(reg.sandbox().allowed_dirs.len(), 1);
    }
}
