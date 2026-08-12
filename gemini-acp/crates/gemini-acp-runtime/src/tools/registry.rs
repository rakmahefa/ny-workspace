//! Registre d'outils : définition, dispatch, résultats.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// Configuration de sandbox partagée par tous les outils d'une session.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub allowed_dirs: Vec<PathBuf>,
    #[allow(dead_code)]
    pub shell_sandbox_enabled: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self { allowed_dirs: Vec::new(), shell_sandbox_enabled: true }
    }
}

/// Descripteur JSON d'un outil, injecté dans le prompt Gemini.
#[derive(Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters_fn: fn() -> Value,
}

impl std::fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDef").field("name", &self.name).field("description", &self.description).finish()
    }
}

impl ToolDef {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "parameters": (self.parameters_fn)(),
        })
    }
}

#[derive(Debug, Clone)]
pub enum ToolResult {
    Ok(String),
    Err(String),
}

impl ToolResult {
    pub fn is_ok(&self) -> bool { matches!(self, ToolResult::Ok(_)) }

    pub fn to_history_text(&self) -> String {
        match self {
            ToolResult::Ok(s) => s.clone(),
            ToolResult::Err(e) => format!("[Erreur] {e}"),
        }
    }
}

/// Interface commune à tous les builtins. La migration conserve volontairement
/// cette abstraction au lieu d'introduire une seconde architecture d'outils.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> &ToolDef;
    async fn execute(&self, args: &Value, cwd: &Path, allowed_dirs: &[PathBuf]) -> ToolResult;
}

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

impl Default for ToolRegistry {
    fn default() -> Self { Self::new() }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new(), sandbox: SandboxConfig::default() }
    }

    #[allow(dead_code)]
    pub fn with_sandbox(sandbox: SandboxConfig) -> Self {
        Self { tools: Vec::new(), sandbox }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        tracing::debug!(name = tool.definition().name, "outil enregistré");
        self.tools.push(tool);
    }

    pub fn builtin() -> Self {
        let mut reg = Self::new();
        reg.register(Box::new(crate::tools::builtin::file::FileReadTool));
        reg.register(Box::new(crate::tools::builtin::file::FileWriteTool));
        reg.register(Box::new(crate::tools::builtin::file::FileEditTool));
        reg.register(Box::new(crate::tools::builtin::shell::ShellExecTool));
        reg.register(Box::new(crate::tools::builtin::search::SearchTool));
        reg
    }

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

    pub fn definitions(&self) -> Vec<Value> {
        self.tools.iter().map(|t| t.definition().to_json()).collect()
    }

    #[allow(dead_code)]
    pub fn sandbox(&self) -> &SandboxConfig { &self.sandbox }

    pub async fn call_async(
        &self,
        name: &str,
        args: &Value,
        cwd: &Path,
        extra_dirs: &[PathBuf],
    ) -> Option<ToolResult> {
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
        ToolDef {
            name: "echo",
            description: "Répète le message.",
            parameters_fn: || json!({
                "type": "object",
                "properties": { "message": { "type": "string" } },
                "required": ["message"]
            }),
        }
    }

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn definition(&self) -> &ToolDef {
            static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
            DEF.get_or_init(echo_def)
        }
        async fn execute(&self, args: &Value, _cwd: &Path, _allowed_dirs: &[PathBuf]) -> ToolResult {
            ToolResult::Ok(args.get("message").and_then(Value::as_str).unwrap_or("").to_string())
        }
    }

    #[test]
    fn registry_builtin_has_filesystem_tools() {
        let reg = ToolRegistry::builtin();
        let names: Vec<&str> = reg.definitions().iter().filter_map(|d| d.get("name").and_then(Value::as_str)).collect();
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
        assert_eq!(result.to_history_text(), "bonjour");
    }

    #[tokio::test]
    async fn call_async_unknown_tool() {
        let reg = ToolRegistry::new();
        assert!(reg.call_async("nonexistent", &json!({}), Path::new("/tmp"), &[]).await.is_none());
    }

    #[test]
    fn tool_result_to_history() {
        assert_eq!(ToolResult::Ok("contenu".into()).to_history_text(), "contenu");
        assert!(ToolResult::Err("fichier introuvable".into()).to_history_text().contains("[Erreur]"));
    }

    #[test]
    fn tool_def_to_json() {
        let def = ToolDef { name: "test", description: "un test", parameters_fn: || json!({"type": "object"}) };
        assert_eq!(def.to_json()["name"], "test");
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
