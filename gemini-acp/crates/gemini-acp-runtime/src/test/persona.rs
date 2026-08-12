use super::*;

fn test_session() -> Session {
    Session::new("sess_test".into(), "/home/dev/projet".into(), vec!["/home/dev/lib".into()], "gemini-3.6-flash")
}

#[test]
fn persona_default_is_coding() { assert_eq!(Persona::default(), Persona::Coding); }

#[test]
fn persona_parse_insensitive() {
    assert_eq!(Persona::from_str_lossy("CODING"), Some(Persona::Coding));
    assert_eq!(Persona::from_str_lossy("creative"), Some(Persona::Creative));
    assert_eq!(Persona::from_str_lossy("brief"), Some(Persona::Concise));
    assert_eq!(Persona::from_str_lossy("invalid"), None);
}

#[test]
fn system_prompt_contains_cwd_and_roots() {
    let p = system_prompt(&test_session(), None);
    assert!(p.contains("CWD: /home/dev/projet"));
    assert!(p.contains("Racines additionnelles: /home/dev/lib"));
}

#[test]
fn system_prompt_coding_has_markdown() {
    let p = system_prompt(&test_session(), Some(Persona::Coding));
    assert!(p.contains("Markdown"));
    assert!(p.contains("code exécutable"));
}

#[test]
fn system_prompt_creative_is_verbose() {
    let p = system_prompt(&test_session(), Some(Persona::Creative));
    assert!(p.contains("analogies"));
    assert!(p.contains("détaillées"));
}

#[test]
fn system_prompt_concise_is_brief() {
    let p = system_prompt(&test_session(), Some(Persona::Concise));
    assert!(p.contains("minimum de texte"));
    assert!(p.contains("Code directement"));
}

#[test]
fn system_prompt_has_constraints() {
    let p = system_prompt(&test_session(), None);
    assert!(p.contains("Ne jamais inventer de fichiers"));
    assert!(p.contains("file_read, file_write, shell_exec, search"));
}

#[test]
fn all_returns_three() { assert_eq!(Persona::all().len(), 3); }
