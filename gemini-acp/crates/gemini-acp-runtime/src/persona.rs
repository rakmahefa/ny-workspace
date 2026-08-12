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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Persona {
    #[default]
    Coding,
    Creative,
    Concise,
}

impl Persona {
    pub fn from_str_lossy(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "coding" | "code" | "default" => Some(Persona::Coding),
            "creative" | "crea" => Some(Persona::Creative),
            "concise" | "brief" => Some(Persona::Concise),
            _ => None,
        }
    }

    pub fn all() -> &'static [Persona] {
        const ALL: &[Persona] = &[Persona::Coding, Persona::Creative, Persona::Concise];
        ALL
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Persona::Coding => "Coding assistant",
            Persona::Creative => "Creative assistant",
            Persona::Concise => "Concise assistant",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Persona::Coding => "Pragmatic coding assistant for Zed. Markdown, executable code.",
            Persona::Creative => "Verbose, analogical, explanatory responses.",
            Persona::Concise => "Ultra-brief responses. Code only, no prose.",
        }
    }

    fn core_instruction(&self) -> &'static str {
        match self {
            Persona::Coding => "\
Réponds en Markdown. Propose du code exécutable quand c'est pertinent. \
Préfère les solutions directes et pragmatiques. \
Si tu utilises un outil, explique brièvement ce que tu fais avant et après.",
            Persona::Creative => "\
Réponds en Markdown avec des explications détaillées. \
Utilise des analogies et des exemples pour clarifier les concepts. \
Propose plusieurs approches quand c'est pertinent. \
Si tu utilises un outil, explique ta démarche en détail.",
            Persona::Concise => "\
Réponds avec le minimum de texte. Pas d'explications sauf si demandé. \
Code directement, sans préambule. \
N'utilise les outils que si c'est strictement nécessaire.",
        }
    }

    fn constraints(&self) -> &'static [&'static str] {
        const CODING: &[&str] = &["Ne jamais inventer de fichiers ou de chemins qui n'existent pas.", "Vérifie les erreurs de compilation si possible.", "Préfère les bibliothèques standards du langage."];
        const CREATIVE: &[&str] = &["Structure les réponses longues avec des titres et sections.", "Inclus des exemples concrets et des cas d'usage."];
        const CONCISE: &[&str] = &["Pas de salutations, pas de conclusions.", "Code commenté uniquement pour les parties non évidentes."];
        match self { Persona::Coding => CODING, Persona::Creative => CREATIVE, Persona::Concise => CONCISE }
    }
}

pub fn system_prompt(session: &Session, persona: Option<Persona>) -> String {
    let p = persona.unwrap_or_default();
    let mut system = String::with_capacity(1024);
    system.push_str(&format!("[System instruction]: tu es un assistant {} intégré à Zed.\n", p.display_name().to_ascii_lowercase()));
    system.push_str(&format!("CWD: {}\n", session.cwd.display()));
    if !session.additional_directories.is_empty() {
        let roots = session.additional_directories.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ");
        system.push_str(&format!("Racines additionnelles: {roots}\n"));
    }
    system.push_str(p.core_instruction());
    system.push_str("\n\n");
    for constraint in p.constraints() { system.push_str("- "); system.push_str(constraint); system.push('\n'); }
    system.push_str("\
- Le code doit être complet et exécutable.\n\
- Utilise les outils (file_read, file_write, shell_exec, search) pour explorer le projet.\n\
- Quand tu modifies un fichier, affiche le chemin et les lignes changées.");
    system.push_str("\n\n");
    system
}

#[cfg(test)]
#[path = "../test/persona.rs"]
mod tests;
