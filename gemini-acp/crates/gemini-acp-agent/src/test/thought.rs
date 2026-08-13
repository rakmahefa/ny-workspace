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
fn thinking_model_emet_la_pensee_progressivement() {
    let mut s = ThoughtSplitter::new(true);
    let (t, m) = s.feed("Voici une pensée suffisamment longue pour dépasser la ");
    assert_eq!(m, "");
    assert!(!t.is_empty());
    assert!(s.has_emitted_thought());

    let (t2, m2) = s.feed("fenêtre de garde\n\n## Réponse\nVoici");
    assert!(!t2.is_empty());
    assert_eq!(m2, "\n\n## Réponse\nVoici");
    assert!(s.has_emitted_thought());

    let (t3, m3) = s.feed(" le résultat");
    assert_eq!(t3, "");
    assert_eq!(m3, " le résultat");
}

#[test]
fn thinking_model_detecte_les_marqueurs_xml_coupes_entre_deltas() {
    let mut s = ThoughtSplitter::new(true);
    let (t1, m1) = s.feed("Réflexion puis </thi");
    assert_eq!(t1, "");
    assert_eq!(m1, "");

    let (t2, m2) = s.feed("nking>Réponse");
    assert_eq!(t2, "Réflexion puis ");
    assert_eq!(m2, "Réponse");

    let (t3, m3) = s.feed(" finale");
    assert_eq!(t3, "");
    assert_eq!(m3, " finale");
}

#[test]
fn thinking_model_consomme_le_marqueur_d_ouverture() {
    let mut s = ThoughtSplitter::new(true);
    let (thought, msg) = s.feed("<thinking>raisonnement");
    assert_eq!(msg, "");
    let (tail, _) = s.flush();
    assert_eq!(format!("{thought}{tail}"), "raisonnement");
}

#[test]
fn thinking_model_flush_emet_reliquat() {
    let mut s = ThoughtSplitter::new(true);
    let (thought, msg) = s.feed("Réflexion courte");
    assert_eq!(thought, "");
    assert_eq!(msg, "");
    let (thought, msg) = s.flush();
    assert_eq!(thought, "Réflexion courte");
    assert_eq!(msg, "");
    assert!(s.has_emitted_thought());
}

#[test]
fn marqueurs_markdown_restent_compatibles() {
    for marker in &["\n\n# Titre", "\n\n### Sous-titre", "\n\n#### Section"] {
        let mut s = ThoughtSplitter::new(true);
        s.feed("Pensée suffisamment longue pour être conservée");
        let (_, msg) = s.feed(marker);
        assert_eq!(msg, *marker);
    }
}

#[test]
fn marqueur_bold_label_detecte() {
    let mut s = ThoughtSplitter::new(true);
    s.feed("Réflexion suffisamment longue pour remplir la fenêtre");
    let (_, msg) = s.feed("\n\n**Réponse**\nVoici");
    assert_eq!(msg, "\n\n**Réponse**\nVoici");
}

#[test]
fn double_newline_seule_ne_termina_pas_la_pensee() {
    let mut s = ThoughtSplitter::new(true);
    s.feed("Pensée suffisamment longue pour dépasser la fenêtre");
    let (thought, msg) = s.feed("\n\nSuite de la pensée");
    assert!(msg.is_empty());
    assert!(!thought.is_empty());
    let (tail, _) = s.flush();
    assert!(tail.contains("Suite de la pensée"));
}

#[test]
fn marqueur_en_plein_milieu_du_flux() {
    let mut s = ThoughtSplitter::new(true);
    s.feed("Pensée partie 1 suffisamment longue pour être émise");
    s.feed(" suite");
    let (_, m0) = s.feed("\n\n");
    assert!(m0.is_empty());
    let (thought, msg) = s.feed("## Réponse\nVoici");
    assert!(thought.is_empty() || thought.len() < 32);
    assert_eq!(msg, "\n\n## Réponse\nVoici");
    assert_eq!(s.feed(" le résultat"), ("".into(), " le résultat".into()));
}
