//! Table des modèles Gemini web (cf. spec §4.4 — vérité =
//! `vendor/gemini-web2api/models.py`).
//!
//! `MODE_CATEGORY` : 1=FAST, 2=THINKING, 3=PRO, 4=AUTO,
//! 5=FAST_DYNAMIC_THINKING, 6=FLASH_LITE.

/// Modèle par défaut (repli en cas de clé inconnue).
pub const DEFAULT_MODEL: &str = "gemini-3.6-flash";

/// Clés sélectionnables (config option ACP / API).
pub const MODEL_KEYS: &[&str] = &[
    "gemini-3.6-flash",
    "gemini-3.5-flash",
    "gemini-3.5-flash-thinking",
    "gemini-3.1-pro",
    "gemini-3.1-pro-enhanced",
    "gemini-auto",
    "gemini-3.5-flash-thinking-lite",
    "gemini-flash-lite",
];

/// Paramètres câble d'un modèle : `mode` → `inner[79]`, `think` → `inner[17]`,
/// `extra` → positions supplémentaires du payload (ex. `{31: 2, 80: 3}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub mode: u32,
    pub think: u32,
    pub extra: Option<Vec<(u32, i64)>>,
}

fn table(key: &str) -> Option<Model> {
    Some(match key {
        "gemini-3.6-flash" | "gemini-3.5-flash" => Model {
            mode: 1,
            think: 4,
            extra: None,
        },
        "gemini-3.5-flash-thinking" => Model {
            mode: 2,
            think: 0,
            extra: None,
        },
        "gemini-3.1-pro" => Model {
            mode: 3,
            think: 4,
            extra: None,
        },
        "gemini-3.1-pro-enhanced" => Model {
            mode: 3,
            think: 4,
            extra: Some(vec![(31, 2), (80, 3)]),
        },
        "gemini-auto" => Model {
            mode: 4,
            think: 4,
            extra: None,
        },
        "gemini-3.5-flash-thinking-lite" => Model {
            mode: 5,
            think: 0,
            extra: None,
        },
        "gemini-flash-lite" => Model {
            mode: 6,
            think: 4,
            extra: None,
        },
        _ => return None,
    })
}

/// Modèle résolu : nom effectif (repli si inconnu), mode, think, extra.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub name: String,
    pub mode: u32,
    pub think: u32,
    pub extra: Option<Vec<(u32, i64)>>,
}

/// Résout une requête de nom de modèle : suffixe `@think=N` → override du
/// niveau de réflexion ; clé inconnue → repli sur `default` (log, pas d'erreur).
/// Erreur si le suffixe `@think=` est invalide ou si plusieurs suffixes sont
/// présents (B15 — on utilise `find` et non `rfind` pour matcher la PREMIÈRE
/// occurrence, et on refuse les noms contenant plus d'un `@think=`).
pub fn resolve(model: &str, default: &str) -> Result<Resolved, String> {
    let mut name = model;
    let mut think_override = None;
    let count = name.matches("@think=").count();
    if count > 1 {
        return Err(format!(
            "Multiple @think= suffixes in model name '{name}' (expected at most one)"
        ));
    }
    if let Some(idx) = name.find("@think=") {
        let level = &name[idx + "@think=".len()..];
        let parsed = level
                .parse::<u32>()
                .map_err(|_| format!("Invalid think level: {level}"))?;
        if parsed > 4 {
            tracing::warn!(requested = parsed, "@think={level} depasse le max (4), borne a 4");
        }
        think_override = Some(parsed.min(4));
        name = &name[..idx];
    }

    let cfg = match table(name) {
        Some(c) => c,
        None => {
            tracing::warn!("Unknown model '{name}', falling back to default '{default}'");
            name = default;
            table(default).expect("default model doit exister dans la table")
        }
    };
    Ok(Resolved {
        name: name.to_string(),
        mode: cfg.mode,
        think: think_override.unwrap_or(cfg.think),
        extra: cfg.extra,
    })
}

/// Renvoie `true` si `mode` correspond à un modèle « thinking » qui émet une
/// chaîne de pensée avant sa réponse (modes 2 et 5 selon la table Gemini).
/// Centralise l'heuristique auparavant dupliquée dans `acp::prompt::turn`
/// (P5) — permet de la tester et de la mettre à jour à un seul endroit.
pub fn is_thinking_mode(mode: u32) -> bool {
    mode == 2 || mode == 5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_normale() {
        let r = resolve("gemini-3.6-flash", DEFAULT_MODEL).unwrap();
        assert_eq!(
            r,
            Resolved {
                name: "gemini-3.6-flash".into(),
                mode: 1,
                think: 4,
                extra: None
            }
        );
    }

    #[test]
    fn extra_pro_enhanced() {
        let r = resolve("gemini-3.1-pro-enhanced", DEFAULT_MODEL).unwrap();
        assert_eq!(r.mode, 3);
        assert_eq!(r.extra, Some(vec![(31, 2), (80, 3)]));
    }

    #[test]
    fn override_think() {
        let r = resolve("gemini-3.6-flash@think=0", DEFAULT_MODEL).unwrap();
        assert_eq!(r.think, 0);
        // Passe-partout du rfind : suffixe au bon endroit.
        let r = resolve("gemini-3.5-flash-thinking-lite@think=2", DEFAULT_MODEL).unwrap();
        assert_eq!(r.think, 2);
        assert_eq!(r.mode, 5);
    }

    #[test]
    fn refuse_multiple_think_suffixes() {
        // Régression B15 : `gemini-3.6-flash@think=2@think=3` doit échouer
        // explicitement, au lieu de parser silencieusement `@think=3` et de
        // laisser `gemini-3.6-flash@think=2` comme nom de modèle (qui échoue
        // la recherche dans la table et tombe sur le repli par défaut).
        let err = resolve("gemini-3.6-flash@think=2@think=3", DEFAULT_MODEL).unwrap_err();
        assert!(err.contains("Multiple @think="), "got: {err}");
    }

    #[test]
    fn is_thinking_mode_justesse() {
        // Régression P5 : centralisation de l'heuristique mode → thinking.
        assert!(!is_thinking_mode(1));
        assert!(is_thinking_mode(2));
        assert!(!is_thinking_mode(3));
        assert!(!is_thinking_mode(4));
        assert!(is_thinking_mode(5));
        assert!(!is_thinking_mode(6));
    }

    #[test]
    fn repli_clé_inconnue() {
        let r = resolve("gpt-4o", DEFAULT_MODEL).unwrap();
        assert_eq!(r.name, DEFAULT_MODEL);
        assert_eq!(r.mode, 1);
    }

    #[test]
    fn think_invalide() {
        let err = resolve("gemini-3.6-flash@think=abc", DEFAULT_MODEL).unwrap_err();
        assert!(err.contains("Invalid think level: abc"));
    }
}
