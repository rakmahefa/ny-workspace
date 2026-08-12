//! Injection de la section outils dans le prompt et formatage
//! de l'historique avec les blocs tool_call / tool_result.
//!
//! Responsabilités :
//! - `tools_section` : construit la section `# Tool Use` injectée après
//!   l'instruction système quand des outils sont disponibles (composition
//!   déléguée à `gemini_acp_config::core::tool_prompt`).
//! - `format_tool_result` : re-export de `tool_result_line` (historique).

use crate::tools::registry::ToolRegistry;
use gemini_acp_config::core::tool_prompt::{tool_use_section, BlockKind, INSTRUCTION_TOOL_CALL};

/// Construit la section `# Tool Use` à injecter dans le prompt.
/// Retourne `None` si le registre est vide.
pub fn tools_section(registry: &ToolRegistry) -> Option<String> {
    let defs = registry.definitions();
    if defs.is_empty() {
        return None;
    }
    Some(tool_use_section(
        BlockKind::ToolCall,
        INSTRUCTION_TOOL_CALL,
        &defs,
        "",
    ))
}

/// Formate un résultat d'outil pour l'historique.
/// Format : `[Tool result for <name>]: <content>` — délégué à
/// `gemini_acp_config::core::tool_prompt::tool_result_line` (re-export conservé pour
/// limiter la surface de changement : appelé par `prompt/turn.rs` et les tests).
pub use gemini_acp_config::core::tool_prompt::tool_result_line as format_tool_result;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::{Tool, ToolDef, ToolRegistry, ToolResult};

    struct DummyTool;

    fn dummy_params() -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    fn dummy_def() -> crate::tools::registry::ToolDef {
        crate::tools::registry::ToolDef {
            name: "dummy",
            description: "Un outil de test.",
            parameters_fn: dummy_params,
        }
    }

    #[async_trait::async_trait]
    impl Tool for DummyTool {
        fn definition(&self) -> &ToolDef {
            static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();
            DEF.get_or_init(dummy_def)
        }
        async fn execute(
            &self,
            _args: &serde_json::Value,
            _cwd: &std::path::Path,
            _allowed_dirs: &[std::path::PathBuf],
        ) -> ToolResult {
            ToolResult::Ok("ok".into())
        }
    }

    #[test]
    fn tools_section_vide_retourne_none() {
        let reg = ToolRegistry::new();
        assert!(tools_section(&reg).is_none());
    }

    #[test]
    fn tools_section_contient_nom_et_description() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(DummyTool));
        let section = tools_section(&reg).unwrap();
        assert!(section.contains("# Tool Use"));
        assert!(section.contains("dummy"));
        assert!(section.contains("Un outil de test."));
        assert!(section.contains("tool_call"));
    }

    #[test]
    fn format_tool_result_text() {
        let r = format_tool_result("file_read", "contenu du fichier");
        assert_eq!(r, "[Tool result for file_read]: contenu du fichier");
    }
}
