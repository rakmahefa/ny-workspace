//! Assemblage du prompt multi-tour (spec §3.4 + refactor M8 §4.2).
//!
//! Responsabilités :
//! - Construction du prompt via `persona::system_prompt`.
//! - Formatage de l'historique avec fenêtre glissante (`format_history`).
//! - Assemblage complet du prompt (`build_prompt`) en O(n + log n) :
//!   précalcul des longueurs par message + binary search sur la fenêtre
//!   optimale (budget chars + max messages).
//! - Injection de la section `# Tool Use` quand les outils sont activés.

use gemini_acp_runtime::tools::ToolRegistry;
use gemini_acp_runtime::persona;
use gemini_acp_runtime::state::{Role, Session};

/// Nombre maximal de messages dans la fenêtre glissante (tour courant inclus).
pub const MAX_MESSAGES: usize = 12;
/// Budget maximal du corps de prompt (chars) — le tour courant n'est jamais tronqué.
pub const MAX_PROMPT_CHARS: usize = 32_000;

/// Construit le prompt complet : instruction système + historique + tour courant
/// (stratégie A — pas de cid/rid/rcid, cf. design_decisions §2).
///
/// Refactor M8 §4.2 : précalcule les longueurs par message + binary search
/// sur la fenêtre optimale. O(n + log n) au lieu de O(n²).
/// Construit le prompt complet avec support des outils.
/// Si `registry` est fourni et `session.tools_enabled`, injecte la section `# Tool Use`.
pub fn build_prompt(session: &Session, registry: Option<&ToolRegistry>) -> String {
    let system = persona::system_prompt(session, None);
    let tools_section = if session.tools_enabled {
        registry.and_then(gemini_acp_runtime::tools::prompt::tools_section)
    } else {
        None
    };
    let system = match tools_section {
        Some(ts) => format!("{system}{ts}\n\n"),
        None => system,
    };
    let n = session.messages.len();
    if n == 0 {
        return system;
    }

    // Précalcul des longueurs par message (tag + ": " + texte + "\n\n").
    let lens: Vec<usize> = session
        .messages
        .iter()
        .map(|(role, text)| {
            let tag = match role {
                Role::User => "[User]",
                Role::Assistant => "[Assistant]",
                Role::Tool => "[Tool result]",
            };
            tag.len() + 2 + text.chars().count() + 2
        })
        .collect();

    // Somme cumulée pour binary search : prefix[i] = lens[0..i].sum()
    let mut prefix = vec![0usize; n + 1];
    for i in 0..n {
        prefix[i + 1] = prefix[i] + lens[i];
    }

    // Fenêtre : [start, n). On veut le plus petit start tel que :
    //   (a) n - start <= MAX_MESSAGES
    //   (b) prefix[n] - prefix[start] <= MAX_PROMPT_CHARS
    //   (c) start < n - 1  (toujours garder le tour courant)
    let min_start_msg = n.saturating_sub(MAX_MESSAGES);
    let budget_ok = |start: usize| prefix[n] - prefix[start] <= MAX_PROMPT_CHARS;

    let mut lo = min_start_msg;
    let mut hi = n - 1; // exclu : on garde au moins le dernier message
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if budget_ok(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let start = lo;

    format!("{system}{}", format_history(session, start))
}

fn format_history(session: &Session, start: usize) -> String {
    let mut out = String::new();
    for (role, text) in session.messages.iter().skip(start) {
        let tag = match role {
            Role::User => "[User]",
            Role::Assistant => "[Assistant]",
            Role::Tool => "[Tool result]", // Déjà formaté par `tools::prompt::format_tool_result`
        };
        out.push_str(tag);
        out.push_str(": ");
        out.push_str(text);
        out.push_str("\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(messages: &[(&str, &str)]) -> Session {
        let mut s = Session::new(
            "sess_test".into(),
            "/home/dev/projet".into(),
            vec![],
            "gemini-3.6-flash",
        );
        s.messages = messages
            .iter()
            .map(|(role, text)| {
                (
                    if *role == "u" {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    (*text).to_string(),
                )
            })
            .collect();
        s
    }

    #[test]
    fn prompt_contient_systeme_et_tour_courant() {
        let s = session(&[("u", "Question 1"), ("a", "Réponse 1"), ("u", "Question 2")]);
        let p = build_prompt(&s, None);
        assert!(p.contains("[System instruction]"));
        assert!(p.contains("CWD: /home/dev/projet"));
        assert!(p.contains("[User]: Question 2"));
        assert!(p.contains("[Assistant]: Réponse 1"));
    }

    #[test]
    fn fenetre_glissante_12_max() {
        let mut s = session(&[]);
        for i in 0..40 {
            s.messages.push((Role::User, format!("Question {i}")));
        }
        let p = build_prompt(&s, None);
        assert!(
            p.contains("Question 39"),
            "le tour courant doit être présent"
        );
        assert!(!p.contains("Question 0"), "le préfixe doit être tronqué");
        assert!(p.matches("[User]").count() <= 12);
    }

    #[test]
    fn troncature_32k_garde_le_tour_courant() {
        let mut msgs = vec![(Role::User, "🚀 premier message très long ".repeat(3_000))];
        for i in 0..4 {
            msgs.push((
                Role::Assistant,
                format!("réponse {i} ") + &"x".repeat(9_000),
            ));
            msgs.push((Role::User, format!("question {i} ") + &"y".repeat(9_000)));
        }
        let mut s = session(&[]);
        s.messages = msgs;
        let p = build_prompt(&s, None);
        assert!(
            p.chars().count() <= 32_000 + 500,
            "budget dépassé: {}",
            p.chars().count()
        );
        assert!(
            p.contains("question 3"),
            "le tour courant ne doit pas être tronqué"
        );
    }

    #[test]
    fn build_prompt_vide_renvoie_juste_systeme() {
        let s = Session::new("s".into(), "/tmp".into(), vec![], "m");
        let p = build_prompt(&s, None);
        assert!(p.contains("[System instruction]"));
        assert!(!p.contains("[User]"));
    }

    #[test]
    fn build_prompt_avec_tools_injecte_section() {
        let s = session(&[]);
        let reg = ToolRegistry::new(); // vide → pas de section
        let p = build_prompt(&s, Some(&reg));
        assert!(!p.contains("# Tool Use"));

        let reg = ToolRegistry::builtin(); // 4 outils
        let p = build_prompt(&s, Some(&reg));
        assert!(p.contains("# Tool Use"));
        assert!(p.contains("file_read"));
        assert!(p.contains("shell_exec"));
    }

    #[test]
    fn build_prompt_tools_disabled_pas_de_section() {
        let mut s = session(&[]);
        s.tools_enabled = false;
        let reg = ToolRegistry::builtin();
        let p = build_prompt(&s, Some(&reg));
        assert!(!p.contains("# Tool Use"));
    }
}
