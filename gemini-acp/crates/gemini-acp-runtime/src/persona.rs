//! Persona Gemini — définition de la personnalité de l'agent.
//!
//! Ce module centralise la construction du prompt système pour l'agent
//! Gemini ACP. Il définit :
//!
//! - **Persona par défaut** : assistant de codage intégré à Zed, concis et
//!   pragmatique, avec des directives claires sur le format de sortie.
//! - **Variantes** : `coding` (défaut), `creative`, `concise`, `custom`.
//! - **Intégration** : utilisé par `prompt/build.rs` via `persona::system_prompt()`.
//!
//! La persona est sélectionnable par l'utilisateur via la clé de modèle
//! suffixée, ex. `gemini-3.6-flash@persona=creative`. Si aucun suffixe n'est
//! fourni, la persona `coding` est utilisée par défaut.

use crate::state::Session;

/// Persona disponibles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Persona {
    /// Assistant de codage pragmatique (défaut).
    #[default]
    Coding,
    /// Mode créatif — réponses plus détaillées, analogies, explications.
    Creative,
    /// Mode concis — réponses ultra-courtes, pas de blabla.
    Concise,
}

impl Persona {
    /// Parse depuis une chaîne (insensible à la casse).
    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "coding" | "code" | "default" => Some(Persona::Coding),
            "creative" | "crea" => Some(Persona::Creative),
            "concise" | "brief" => Some(Persona::Concise),
            _ => None,
        }
    }

    /// Liste des personas disponibles (pour la config option).
    pub fn all() -> &'static [Persona] {
        const ALL: &[Persona] = &[Persona::Coding, Persona::Creative, Persona::Concise];
        ALL
    }

    /// Nom d'affichage.
    pub fn display_name(&self) -> &'static str {
        match self {
            Persona::Coding => "Coding assistant",
            Persona::Creative => "Creative assistant",
            Persona::Concise => "Concise assistant",
        }
    }

    /// Description courte.
    pub fn description(&self) -> &'static str {
        match self {
            Persona::Coding => "Pragmatic coding assistant for Zed. Markdown, executable code.",
            Persona::Creative => "Verbose, analogical, explanatory responses.",
            Persona::Concise => "Ultra-brief responses. Code only, no prose.",
        }
    }

    /// Instruction système spécifique à la persona.
    fn core_instruction(&self) -> &'static str {
        match self {
            Persona::Coding => {
                "\
Réponds en Markdown. Propose du code exécutable quand c'est pertinent. \
Préfère les solutions directes et pragmatiques. \
Si tu utilises un outil, explique brièvement ce que tu fais avant et après."
            }
            Persona::Creative => {
                "\
Réponds en Markdown avec des explications détaillées. \
Utilise des analogies et des exemples pour clarifier les concepts. \
Propose plusieurs approches quand c'est pertinent. \
Si tu utilises un outil, explique ta démarche en détail."
            }
            Persona::Concise => {
                "\
Réponds avec le minimum de texte. Pas d'explications sauf si demandé. \
Code directement, sans préambule. \
N'utilise les outils que si c'est strictement nécessaire."
            }
        }
    }

    /// Constraints spécifiques à la persona (ajoutées à la fin du système).
    fn constraints(&self) -> &'static [&'static str] {
        const CODING: &[&str] = &[
            "Ne jamais inventer de fichiers ou de chemins qui n'existent pas.",
            "Vérifie les erreurs de compilation si possible.",
            "Préfère les bibliothèques standards du langage.",
        ];
        const CREATIVE: &[&str] = &[
            "Structure les réponses longues avec des titres et sections.",
            "Inclus des exemples concrets et des cas d'usage.",
        ];
        const CONCISE: &[&str] = &[
            "Pas de salutations, pas de conclusions.",
            "Code commenté uniquement pour les parties non évidentes.",
        ];
        match self {
            Persona::Coding => CODING,
            Persona::Creative => CREATIVE,
            Persona::Concise => CONCISE,
        }
    }
}

/// Construit le prompt système complet en combinant :
/// 1. L'en-tête fixe (identité de l'agent).
/// 2. Le contexte (CWD, racines additionnelles).
/// 3. L'instruction de persona.
/// 4. Les contraintes.
/// 5. Les directives de format de sortie.
pub fn system_prompt(session: &Session, persona: Option<Persona>) -> String {
    let p = persona.unwrap_or_default();
    let mut system = String::with_capacity(1024);

    // En-tête.
    system.push_str(&format!(
        "[System instruction]: tu es un assistant {} intégré à Zed.\n",
        p.display_name().to_ascii_lowercase()
    ));

    // Contexte.
    system.push_str(&format!("CWD: {}\n", session.cwd.display()));
    if !session.additional_directories.is_empty() {
        let roots = session
            .additional_directories
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        system.push_str(&format!("Racines additionnelles: {roots}\n"));
    }

    // Instruction persona.
    system.push_str(p.core_instruction());
    system.push_str("\n\n");

    // Contraintes.
    for constraint in p.constraints() {
        system.push_str("- ");
        system.push_str(constraint);
        system.push('\n');
    }

    // Directives de format.
    system.push_str(
        "\
- Le code doit être complet et exécutable.\n\
- Utilise les outils (file_read, file_write, shell_exec, search) pour explorer le projet.\n\
- Quand tu modifies un fichier, affiche le chemin et les lignes changées.",
    );
    system.push_str("\n\n");

    system
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session() -> Session {
        Session::new(
            "sess_test".into(),
            "/home/dev/projet".into(),
            vec!["/home/dev/lib".into()],
            "gemini-3.6-flash",
        )
    }

    #[test]
    fn persona_default_is_coding() {
        assert_eq!(Persona::default(), Persona::Coding);
    }

    #[test]
    fn persona_parse_insensitive() {
        assert_eq!(Persona::from_str_lossy("CODING"), Some(Persona::Coding));
        assert_eq!(Persona::from_str_lossy("creative"), Some(Persona::Creative));
        assert_eq!(Persona::from_str_lossy("brief"), Some(Persona::Concise));
        assert_eq!(Persona::from_str_lossy("invalid"), None);
    }

    #[test]
    fn system_prompt_contains_cwd_and_roots() {
        let s = test_session();
        let p = system_prompt(&s, None);
        assert!(p.contains("CWD: /home/dev/projet"));
        assert!(p.contains("Racines additionnelles: /home/dev/lib"));
    }

    #[test]
    fn system_prompt_coding_has_markdown() {
        let s = test_session();
        let p = system_prompt(&s, Some(Persona::Coding));
        assert!(p.contains("Markdown"));
        assert!(p.contains("code exécutable"));
    }

    #[test]
    fn system_prompt_creative_is_verbose() {
        let s = test_session();
        let p = system_prompt(&s, Some(Persona::Creative));
        assert!(p.contains("analogies"));
        assert!(p.contains("détaillées"));
    }

    #[test]
    fn system_prompt_concise_is_brief() {
        let s = test_session();
        let p = system_prompt(&s, Some(Persona::Concise));
        assert!(p.contains("minimum de texte"));
        assert!(p.contains("Code directement"));
    }

    #[test]
    fn system_prompt_has_constraints() {
        let s = test_session();
        let p = system_prompt(&s, None);
        assert!(p.contains("Ne jamais inventer de fichiers"));
        assert!(p.contains("file_read, file_write, shell_exec, search"));
    }

    #[test]
    fn all_returns_three() {
        assert_eq!(Persona::all().len(), 3);
    }
}
