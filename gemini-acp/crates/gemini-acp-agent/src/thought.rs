//! Heuristique de séparation pensée/réponse pour les modèles thinking
//! (refactor M7 §3.2).
//!
//! Le backend Gemini web ne distingue pas formellement pensée et réponse dans
//! le flux `StreamGenerate` — tout arrive dans les mêmes `segments` de
//! `candidate[1]`. Cependant, les modèles thinking (`gemini-3.5-flash-thinking`
//! mode 2, `gemini-3.5-flash-thinking-lite` mode 5) produisent typiquement un
//! bloc de raisonnement avant la réponse finale, souvent suivi d'un marqueur
//! comme `## Réponse` ou une double newline + titre Markdown.
//!
//! Stratégie : si `resolved.mode ∈ {2, 5}`, considérer le début du flux comme
//! du raisonnement jusqu'à détecter un marqueur de fin de pensée. Émettre
//! alors `AgentThoughtChunk` pour le buffer accumulé, puis basculer en mode
//! `AgentMessageChunk` normal. Si aucun marqueur n'est détecté, tout est émis
//! en `AgentThoughtChunk` (cas extrême où le modèle ne fait que réfléchir).

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, MessageId, SessionId, SessionNotification, SessionUpdate,
    TextContent,
};
use agent_client_protocol::{Client, ConnectionTo, Error as AcpError};

/// État du découpeur pensée/réponse pour un tour.
#[derive(Debug, Default)]
pub struct ThoughtSplitter {
    /// true tant qu'on est dans la phase de pensée.
    in_thought: bool,
    /// Buffer accumulant les deltas pendant la phase de pensée.
    thought_buffer: String,
    /// true si on a déjà émis au moins un AgentThoughtChunk.
    has_emitted_thought: bool,
}

impl ThoughtSplitter {
    /// Crée un splitter. `is_thinking_model` = true si `resolved.mode ∈ {2, 5}`.
    pub fn new(is_thinking_model: bool) -> Self {
        Self {
            in_thought: is_thinking_model,
            thought_buffer: String::new(),
            has_emitted_thought: false,
        }
    }

    /// Ingère un delta de texte. Retourne `(thought_delta, message_delta)` :
    /// - `thought_delta` : texte à émettre en `AgentThoughtChunk` (peut être vide).
    /// - `message_delta` : texte à émettre en `AgentMessageChunk` (peut être vide).
    ///
    /// Algorithme :
    /// - Si pas en phase de pensée → tout va dans `message_delta`.
    /// - Si en phase de pensée → on bufferise jusqu'à détecter un marqueur de fin.
    ///   - Marqueur : `\n\n## ` ou `\n\n# ` (titre Markdown) ou `\n\n**` (bold).
    ///   - À la détection : on émet tout le buffer cumulé en `thought_delta`,
    ///     puis le reste (marqueur inclus) en `message_delta`, et on bascule.
    /// - Si on n'est plus en pensée mais qu'il reste du buffer non émis
    ///   (cas where le tour se termine sans marqueur), `flush()` l'émettra.
    pub fn feed(&mut self, delta: &str) -> (String, String) {
        if !self.in_thought {
            return (String::new(), delta.to_string());
        }

        self.thought_buffer.push_str(delta);

        // Cherche un marqueur de fin de pensée.
        if let Some(marker_idx) = find_thought_end(&self.thought_buffer) {
            let thought = self.thought_buffer[..marker_idx].to_string();
            let message = self.thought_buffer[marker_idx..].to_string();
            self.thought_buffer.clear();
            self.in_thought = false;
            self.has_emitted_thought = true;
            (thought, message)
        } else {
            // Toujours en pensée — rien à émettre pour l'instant.
            (String::new(), String::new())
        }
    }

    /// Termine le tour : si on est toujours en pensée (pas de marqueur détecté),
    /// émet tout le buffer restant comme pensée. Si le buffer est vide et qu'on
    /// n'a jamais émis de pensée, retourne vide (le flux était probablement vide).
    pub fn flush(&mut self) -> (String, String) {
        if self.in_thought && !self.thought_buffer.is_empty() {
            let thought = std::mem::take(&mut self.thought_buffer);
            self.in_thought = false;
            self.has_emitted_thought = true;
            (thought, String::new())
        } else {
            (String::new(), String::new())
        }
    }

    /// true si au moins un `AgentThoughtChunk` a été émis.
    #[allow(dead_code)]
    pub fn has_emitted_thought(&self) -> bool {
        self.has_emitted_thought
    }
}

/// Détecte la position du marqueur de fin de pensée dans un buffer cumulé.
/// Marqueurs supportés (priorité décroissante) :
/// - `\n\n## ` (titre Markdown H2 — le plus courant)
/// - `\n\n# ` (titre Markdown H1)
/// - `\n\n### ` (titre Markdown H3)
/// - `\n\n**` suivi d'un mot (bold — souvent un label « Réponse »)
///
/// Retourne l'index du début du marqueur (le `\n` initial) dans le buffer,
/// ou `None` si aucun marqueur n'a été trouvé.
fn find_thought_end(buffer: &str) -> Option<usize> {
    // Cherche `\n\n` suivi d'un marqueur de titre ou de bold.
    let mut search_from = 0;
    while let Some(dnl_idx) = buffer[search_from..].find("\n\n") {
        let abs_idx = search_from + dnl_idx;
        let after = &buffer[abs_idx + 2..];
        if after.starts_with("## ")
            || after.starts_with("# ")
            || after.starts_with("### ")
            || after.starts_with("#### ")
            || (after.starts_with("**")
                && after.len() > 2
                && after[2..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric()))
        {
            return Some(abs_idx);
        }
        search_from = abs_idx + 1;
    }
    None
}

/// Émet un `AgentThoughtChunk` pour le texte donné.
pub async fn notify_thought(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    message_id: &MessageId,
    text: String,
) -> Result<(), AcpError> {
    if text.is_empty() {
        return Ok(());
    }
    cx.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
                .message_id(message_id.clone()),
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_thinking_model_emit_tout_en_message() {
        let mut s = ThoughtSplitter::new(false);
        let (thought, msg) = s.feed("Bonjour");
        assert_eq!(thought, "");
        assert_eq!(msg, "Bonjour");
        let (thought, msg) = s.feed(" le monde");
        assert_eq!(thought, "");
        assert_eq!(msg, " le monde");
        let (t, m) = s.flush();
        assert_eq!(t, "");
        assert_eq!(m, "");
        assert!(!s.has_emitted_thought());
    }

    #[test]
    fn thinking_model_bufferise_puis_emet_sur_marqueur() {
        let mut s = ThoughtSplitter::new(true);
        // Phase de pensée : rien n'est émis tant qu'on n'a pas de marqueur.
        let (thought, msg) = s.feed("Je réfléchis au problème");
        assert_eq!(thought, "");
        assert_eq!(msg, "");
        let (thought, msg) = s.feed(" et voici ma réponse");
        assert_eq!(thought, "");
        assert_eq!(msg, "");
        // Marqueur de fin : `\n\n## `
        let (thought, msg) = s.feed("\n\n## Réponse\nVoici");
        assert_eq!(thought, "Je réfléchis au problème et voici ma réponse");
        assert_eq!(msg, "\n\n## Réponse\nVoici");
        assert!(s.has_emitted_thought());
        // Après le marqueur, on est en mode message.
        let (thought, msg) = s.feed(" le résultat");
        assert_eq!(thought, "");
        assert_eq!(msg, " le résultat");
    }

    #[test]
    fn thinking_model_flush_emet_buffer_restant() {
        // Pas de marqueur détecté → flush émet tout en pensée.
        let mut s = ThoughtSplitter::new(true);
        s.feed("Longue réflexion sans marqueur de fin");
        let (thought, msg) = s.flush();
        assert_eq!(thought, "Longue réflexion sans marqueur de fin");
        assert_eq!(msg, "");
        assert!(s.has_emitted_thought());
    }

    #[test]
    fn marqueur_h1_h3_h4_detectes() {
        for marker in &["\n\n# Titre", "\n\n### Sous-titre", "\n\n#### Section"] {
            let mut s = ThoughtSplitter::new(true);
            s.feed("Pensée");
            let (thought, msg) = s.feed(marker);
            assert_eq!(thought, "Pensée", "marqueur {} non détecté", marker);
            assert_eq!(msg, *marker);
        }
    }

    #[test]
    fn marqueur_bold_label_detecte() {
        let mut s = ThoughtSplitter::new(true);
        s.feed("Réflexion");
        let (thought, msg) = s.feed("\n\n**Réponse**\nVoici");
        assert_eq!(thought, "Réflexion");
        assert_eq!(msg, "\n\n**Réponse**\nVoici");
    }

    #[test]
    fn double_newline_seule_pas_un_marqueur() {
        // `\n\n` suivi de texte simple n'est PAS un marqueur.
        let mut s = ThoughtSplitter::new(true);
        s.feed("Pensée");
        let (thought, msg) = s.feed("\n\nSuite de la pensée");
        assert_eq!(thought, "");
        assert_eq!(msg, "");
        // Le buffer cumulé contient maintenant tout.
        let (thought, msg) = s.flush();
        assert_eq!(thought, "Pensée\n\nSuite de la pensée");
        assert_eq!(msg, "");
    }

    #[test]
    fn marqueur_en_plein_milieu_du_flux() {
        let mut s = ThoughtSplitter::new(true);
        // Le marqueur peut arriver en plusieurs morceaux. Le `\n\n` seul ne
        // déclenche pas (after est vide), puis `## Réponse\n` complète le marqueur.
        let (t, m) = s.feed("Pensée partie 1");
        assert_eq!(t, "");
        assert_eq!(m, "");
        let (t, m) = s.feed(" suite");
        assert_eq!(t, "");
        assert_eq!(m, "");
        let (t, m) = s.feed("\n\n");
        assert_eq!(t, ""); // after est vide → pas encore un marqueur
        assert_eq!(m, "");
        // Dès que `## ` arrive, le marqueur `\n\n## ` est complet → déclenchement.
        let (thought, msg) = s.feed("## Réponse\nVoici");
        assert_eq!(thought, "Pensée partie 1 suite");
        assert_eq!(msg, "\n\n## Réponse\nVoici");
        // Après le marqueur, on est en mode message.
        let (t, m) = s.feed(" le résultat");
        assert_eq!(t, "");
        assert_eq!(m, " le résultat");
    }
}
