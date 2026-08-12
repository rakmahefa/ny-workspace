//! Section `# Tool Use` partagée entre les crates (refactor B — spec).
//!
//! Les 3 convertisseurs (`acp::tools::prompt`, `gemini-web2api::convert::openai`,
//! `gemini-web2api::convert::google`) construisaient chacun une section quasi
//! identique : header + instruction + liste de définitions (JSON pretty) +
//! contrainte optionnelle. Ce module centralise la composition ; seuls changent
//! le label de fence, le texte d'instruction, la liste de déf et le suffixe.

use serde_json::Value;

/// Label du bloc de sortie que le modèle doit produire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// Bloc ` ```tool_call ` (format OpenAI / agent ACP).
    ToolCall,
    /// Bloc ` ```function_call ` (format Google natif).
    FunctionCall,
}

/// Instruction du format `tool_call` — texte actuel de
/// `acp::tools::prompt::INSTRUCTION`. Se termine par `Available tools:`
/// (sans saut de ligne : le helper ajoute la séparation).
pub const INSTRUCTION_TOOL_CALL: &str = "# Tool Use\n\n\
You have access to tools that execute in the user's local environment. \
To call a tool, respond with:\n\
```tool_call\n{\"name\": \"<tool_name>\", \"arguments\": {<arguments>}}\n```\
You may call multiple tools with multiple blocks in a single response. \
After receiving a [Tool result for ...], use that data to answer the user. \
Only use tool_call blocks when the user's request requires it.\n\n\
Available tools:";

/// Instruction du format `function_call` — texte actuel de
/// `gemini-web2api::convert::google::google_tools_section`. Se termine par
/// `Available tools:\n` (saut de ligne final inclus).
pub const INSTRUCTION_FUNCTION_CALL: &str = "# Tool Use\n\n\
You can call the following tools to help accomplish tasks. \
These tools connect to the user's local environment and will execute when called.\n\n\
Call format (use this exact format):\n\
```function_call\n{\"name\": \"<tool_name>\", \"args\": {<arguments>}}\n```\n\n\
When calling tools:\n\
- Output ONLY the function_call block(s), nothing else\n\
- You may call multiple tools with multiple blocks\n\
- After receiving a [Tool result for ...], use that data to answer the user\n\n\
Available tools:\n";

/// Construit la section `# Tool Use` commune :
/// header + instruction + déf (JSON pretty) + contrainte optionnelle.
///
/// Contrats :
/// - Le fence est choisi par `block_kind` (`tool_call` vs `function_call`) et
///   documenté dans l'instruction fournie (vérifié en debug).
/// - `defs` est déjà normalisé par l'appelant (objet `{name, description,
///   parameters}`) — la normalisation OpenAI/Google reste dans chaque convertisseur.
/// - Retourne une chaîne prête à concaténer : l'appelant décide de l'absence
///   de section (il n'appelle pas le helper si `defs` est vide).
pub fn tool_use_section(
    block_kind: BlockKind,
    instruction: &str,
    defs: &[Value],
    constraint: &str,
) -> String {
    let fence = match block_kind {
        BlockKind::ToolCall => "tool_call",
        BlockKind::FunctionCall => "function_call",
    };
    debug_assert!(
        instruction.contains(&format!("```{fence}")),
        "l'instruction doit documenter le fence ```{fence}"
    );
    let defs_json = serde_json::to_string_pretty(defs).unwrap_or_else(|_| "[]".into());
    // Séparation instruction/déf : l'instruction se termine par
    // `Available tools:` avec ou sans saut de ligne final — on normalise pour
    // rester identique aux sorties actuelles des 3 convertisseurs.
    let sep = if instruction.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    format!("{instruction}{sep}{defs_json}{constraint}")
}

/// Formate un résultat d'outil pour l'historique :
/// `[Tool result for <name>]: <content>`.
pub fn tool_result_line(name: &str, content: &str) -> String {
    format!("[Tool result for {name}]: {content}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_use_section_fence_selon_block_kind() {
        let defs = vec![json!({"name": "lire", "description": "lit un fichier", "parameters": {}})];
        let tool_call = tool_use_section(BlockKind::ToolCall, INSTRUCTION_TOOL_CALL, &defs, "");
        assert!(tool_call.contains("# Tool Use"));
        assert!(tool_call.contains("```tool_call"));
        assert!(tool_call.contains("\"lire\""));
        let function_call = tool_use_section(
            BlockKind::FunctionCall,
            INSTRUCTION_FUNCTION_CALL,
            &defs,
            "",
        );
        assert!(function_call.contains("```function_call"));
        assert!(function_call.contains("\"lire\""));
    }

    #[test]
    fn tool_use_section_inclut_instruction_defs_et_contrainte() {
        let defs = vec![json!({"name": "lire", "description": "lit"})];
        let s = tool_use_section(
            BlockKind::ToolCall,
            INSTRUCTION_TOOL_CALL,
            &defs,
            "\n\nIMPORTANT: contrainte",
        );
        assert!(s.starts_with("# Tool Use"));
        assert!(s.contains("You have access to tools"));
        assert!(s.contains("\"name\": \"lire\""));
        assert!(s.ends_with("IMPORTANT: contrainte"));
    }

    #[test]
    fn tool_result_line_formatage_exact() {
        assert_eq!(
            tool_result_line("file_read", "contenu du fichier"),
            "[Tool result for file_read]: contenu du fichier"
        );
    }
}
