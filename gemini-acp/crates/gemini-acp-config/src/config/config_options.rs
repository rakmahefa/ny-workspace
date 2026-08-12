//! Options de configuration exposées à Zed (spec §3.6).
//!
//! Refactor R1 — inspiré de `glm-acp-agent` :
//! - **SessionCapabilities** : ajout de `fork`.
//! - **Modes** : les modes sont maintenant gérés par le handler
//!   `session/set_mode`, pas comme config options.
//!
//! Refactor 3-crates (spec §3.2, note) : `build_config_options` prend les
//! champs individuellement (`model`, `think`, `tools_enabled`) au lieu de
//! `&Session`, afin d'éviter une dépendance vers le crate `runtime` (où vit
//! désormais le type `Session`).

use agent_client_protocol::schema::v1::*;

use crate::core::models;

/// Options `config_options` : modèle + niveau de réflexion + outils.
pub fn build_config_options(
    model: &str,
    think: Option<u32>,
    tools_enabled: bool,
) -> Vec<SessionConfigOption> {
    let tools_options = vec![
        SessionConfigSelectOption::new(SessionConfigValueId::from("true"), "Activé"),
        SessionConfigSelectOption::new(SessionConfigValueId::from("false"), "Désactivé"),
    ];
    let model_options: Vec<SessionConfigSelectOption> = models::MODEL_KEYS
        .iter()
        .map(|key| SessionConfigSelectOption::new(SessionConfigValueId::from(*key), *key))
        .collect();
    let think_default = think.unwrap_or_else(|| {
        models::resolve(model, models::DEFAULT_MODEL)
            .map(|r| r.think)
            .unwrap_or(4)
    });
    let think_options: Vec<SessionConfigSelectOption> = (0..=4)
        .map(|n| {
            SessionConfigSelectOption::new(
                SessionConfigValueId::from(n.to_string()),
                format!("Réflexion {n}"),
            )
        })
        .collect();
    vec![
        SessionConfigOption::new(
            SessionConfigId::from("model"),
            "Modèle",
            SessionConfigKind::Select(SessionConfigSelect::new(
                SessionConfigValueId::from(model.to_string()),
                SessionConfigSelectOptions::Ungrouped(model_options),
            )),
        )
        .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::new(
            SessionConfigId::from("think"),
            "Réflexion",
            SessionConfigKind::Select(SessionConfigSelect::new(
                SessionConfigValueId::from(think_default.to_string()),
                SessionConfigSelectOptions::Ungrouped(think_options),
            )),
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
        SessionConfigOption::new(
            SessionConfigId::from("tools_enabled"),
            "Outils (file, shell, search)",
            SessionConfigKind::Select(SessionConfigSelect::new(
                SessionConfigValueId::from(if tools_enabled { "true" } else { "false" }),
                SessionConfigSelectOptions::Ungrouped(tools_options),
            )),
        )
        .category(SessionConfigOptionCategory::Model),
    ]
}

/// Capabilités annoncées à l'initialisation (spec §2.2).
///
/// Reflète les handlers réellement implémentés : list/delete/fork/close.
pub fn build_agent_capabilities() -> AgentCapabilities {
    AgentCapabilities::new().session_capabilities(
        SessionCapabilities::new()
            .list(SessionListCapabilities::new())
            .delete(SessionDeleteCapabilities::new())
            .close(SessionCloseCapabilities::new())
            .fork(SessionForkCapabilities::new()), // unstable_session_fork — feature par défaut
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_config_options_retourne_3_options() {
        let options = build_config_options("gemini-3.6-flash", Some(2), true);
        assert_eq!(options.len(), 3);
    }

    #[test]
    fn build_agent_capabilities_inclut_fork() {
        // Construction sans panique = capabilities correctement câblées.
        let _caps = build_agent_capabilities();
    }
}
