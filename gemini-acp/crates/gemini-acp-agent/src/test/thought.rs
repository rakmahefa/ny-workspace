use super::*;

#[test]
fn non_thinking_model_emit_tout_en_message() {
    let mut s = ThoughtSplitter::new(false);
    let (thought, msg) = s.feed("Bonjour");
    assert_eq!(thought, ""); assert_eq!(msg, "Bonjour");
    let (thought, msg) = s.feed(" le monde");
    assert_eq!(thought, ""); assert_eq!(msg, " le monde");
    let (t, m) = s.flush(); assert_eq!(t, ""); assert_eq!(m, ""); assert!(!s.has_emitted_thought());
}

#[test]
fn thinking_model_bufferise_puis_emet_sur_marqueur() {
    let mut s = ThoughtSplitter::new(true);
    assert_eq!(s.feed("Je réfléchis au problème"), ("".into(), "".into()));
    assert_eq!(s.feed(" et voici ma réponse"), ("".into(), "".into()));
    let (thought, msg) = s.feed("\n\n## Réponse\nVoici");
    assert_eq!(thought, "Je réfléchis au problème et voici ma réponse");
    assert_eq!(msg, "\n\n## Réponse\nVoici");
    assert!(s.has_emitted_thought());
    assert_eq!(s.feed(" le résultat"), ("".into(), " le résultat".into()));
}

#[test]
fn thinking_model_flush_emet_buffer_restant() {
    let mut s = ThoughtSplitter::new(true); s.feed("Longue réflexion sans marqueur de fin");
    let (thought, msg) = s.flush();
    assert_eq!(thought, "Longue réflexion sans marqueur de fin"); assert_eq!(msg, ""); assert!(s.has_emitted_thought());
}

#[test]
fn marqueur_h1_h3_h4_detectes() {
    for marker in &["\n\n# Titre", "\n\n### Sous-titre", "\n\n#### Section"] {
        let mut s = ThoughtSplitter::new(true); s.feed("Pensée");
        let (thought, msg) = s.feed(marker); assert_eq!(thought, "Pensée"); assert_eq!(msg, *marker);
    }
}

#[test]
fn marqueur_bold_label_detecte() {
    let mut s = ThoughtSplitter::new(true); s.feed("Réflexion");
    let (thought, msg) = s.feed("\n\n**Réponse**\nVoici");
    assert_eq!(thought, "Réflexion"); assert_eq!(msg, "\n\n**Réponse**\nVoici");
}

#[test]
fn double_newline_seule_pas_un_marqueur() {
    let mut s = ThoughtSplitter::new(true); s.feed("Pensée");
    assert_eq!(s.feed("\n\nSuite de la pensée"), ("".into(), "".into()));
    let (thought, msg) = s.flush(); assert_eq!(thought, "Pensée\n\nSuite de la pensée"); assert_eq!(msg, "");
}

#[test]
fn marqueur_en_plein_milieu_du_flux() {
    let mut s = ThoughtSplitter::new(true);
    s.feed("Pensée partie 1"); s.feed(" suite"); assert_eq!(s.feed("\n\n"), ("".into(), "".into()));
    let (thought, msg) = s.feed("## Réponse\nVoici");
    assert_eq!(thought, "Pensée partie 1 suite"); assert_eq!(msg, "\n\n## Réponse\nVoici");
    assert_eq!(s.feed(" le résultat"), ("".into(), " le résultat".into()));
}
