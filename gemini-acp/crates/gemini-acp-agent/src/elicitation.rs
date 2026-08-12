//! Helper d'elicitation pour demander une clarification structurée à
//! l'utilisateur (refactor M10 §7.2).
//!
//! **Statut** : l'elicitation est marquée **UNSTABLE** dans le schema ACP 1.5.0
//! (feature `unstable_elicitation` du crate `agent-client-protocol-schema`).
//! Ce module fournit un helper prêt à l'emploi mais n'est pas activé par défaut
//! — pour l'utiliser, activez la feature `elicitation` dans `acp/Cargo.toml` :
//!
//! ```toml
//! [features]
//! elicitation = ["agent-client-protocol/unstable_elicitation"]
//! ```
//!
//! ## Cas d'usage
//!
//! - L'utilisateur dit « refactor ce fichier » sans préciser le style →
//!   l'agent élicite « Quel style ? (functional / OOP / data-oriented) ».
//! - L'utilisateur dit « génère des tests » → l'agent élicite « Quel
//!   framework ? (cargo test / proptest / criterion) » et « Combien de cas ? ».
//!
//! ## Limitation
//!
//! Gemini web ne supporte pas nativement l'elicitation — c'est l'agent qui
//! doit décider quand elicitier. Stratégie recommandée : si le premier message
//! utilisateur contient des mots-clés vagues (« refactor », « optimise »,
//! « teste ») sans précision, éliciter avant de lancer le tour Gemini.

#![cfg(feature = "elicitation")]

use std::collections::BTreeMap;

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAction,
    ElicitationContentValue, ElicitationFormMode, ElicitationPropertySchema, ElicitationSchema,
    ElicitationSessionScope, SessionId,
};
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError};

/// Demande une clarification structurée à l'utilisateur via le client ACP.
///
/// `message` : description humaine de ce qui est demandé.
/// `properties` : map nom du champ → schéma de propriété (type, description, etc.).
/// `required` : liste des noms de champs obligatoires.
///
/// Retourne :
/// - `Ok(Some(content))` si l'utilisateur a accepté et fourni les données.
/// - `Ok(None)` si l'utilisateur a refusé ou annulé.
/// - `Err(AcpError)` si la requête a échoué (client ne supporte pas l'elicitation, etc.).
pub async fn elicit_clarification(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    message: &str,
    properties: BTreeMap<String, ElicitationPropertySchema>,
    required: Vec<String>,
) -> Result<Option<BTreeMap<String, ElicitationContentValue>>, AcpError> {
    let scope = ElicitationSessionScope::new(session_id.clone());
    // Construit le schéma en ajoutant chaque propriété via le builder.
    let mut schema = ElicitationSchema::new();
    for (name, prop_schema) in properties {
        let is_required = required.contains(&name);
        schema = schema.property(name, prop_schema, is_required);
    }
    let mode = ElicitationFormMode::new(scope, schema);
    let request = CreateElicitationRequest::new(mode, message.to_string());

    let response: CreateElicitationResponse = cx.send_request(request).block_task().await?;

    match response.action {
        ElicitationAction::Accept(accept) => Ok(accept.content),
        ElicitationAction::Decline | ElicitationAction::Cancel => Ok(None),
        // Wildcard pour variants futures (#[non_exhaustive]).
        _ => Ok(None),
    }
}

/// Détecte si un message utilisateur est « vague » et mérite une elicitation.
/// Mots-clés vagues : refactor, optimise, teste, améliore, simplifie, etc.
pub fn is_vague_prompt(message: &str) -> bool {
    let lower = message.to_lowercase();
    let vague_keywords = [
        "refactor",
        "optimise",
        "optimize",
        "teste",
        "test ",
        "améliore",
        "ameliore",
        "simplifie",
        "nettoie",
        "réécris",
        "reecris",
    ];
    let has_vague = vague_keywords.iter().any(|k| lower.contains(k));
    // Heuristique : prompt court (< 80 chars) + mot-clé vague = demande
    // probablement imprécise.
    has_vague && message.chars().count() < 80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detecte_prompt_vague_refactor() {
        assert!(is_vague_prompt("Refactor ce fichier"));
        assert!(is_vague_prompt("Optimise le code"));
        assert!(is_vague_prompt("Teste la fonction"));
    }

    #[test]
    fn prompt_precis_pas_vague() {
        assert!(!is_vague_prompt("Refactor la fonction `parse_config` en utilisant le pattern builder, en gardant la compatibilité avec l'API existante"));
        assert!(!is_vague_prompt("Ajoute un test unitaire pour la fonction `build_prompt` dans `crates/acp/src/prompt.rs`"));
    }

    #[test]
    fn prompt_sans_mot_cle_pas_vague() {
        assert!(!is_vague_prompt("Bonjour, comment vas-tu ?"));
        assert!(!is_vague_prompt("Explique-moi le pattern MVP en Rust"));
    }
}
