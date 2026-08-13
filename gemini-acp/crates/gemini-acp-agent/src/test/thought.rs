use super::*;

#[test]
fn non_thinking_model_emits_response_events_directly() {
    let mut stream = ThoughtStream::new(false);
    assert_eq!(
        stream.feed("Bonjour"),
        vec![ThoughtEvent::ResponseChunk("Bonjour".into())]
    );
    assert_eq!(
        stream.feed(" le monde"),
        vec![ThoughtEvent::ResponseChunk(" le monde".into())]
    );
    assert_eq!(stream.phase(), ThoughtPhase::Response);
}

#[test]
fn thinking_model_emits_thought_then_response() {
    let mut stream = ThoughtStream::new(true);
    let first = stream.feed("Voici une pensée suffisamment longue pour dépasser la ");
    assert_eq!(first.len(), 1);
    assert!(matches!(first[0], ThoughtEvent::ThoughtChunk(_)));

    let second = stream.feed("fenêtre de garde\n\n## Réponse\nVoici");
    assert_eq!(
        second,
        vec![
            ThoughtEvent::ThoughtChunk("fenêtre de garde".into()),
            ThoughtEvent::ThoughtEnd,
            ThoughtEvent::ResponseChunk("\n\n## Réponse\nVoici".into()),
        ]
    );
    assert_eq!(stream.phase(), ThoughtPhase::Response);
    assert!(stream.has_emitted_thought());

    assert_eq!(
        stream.feed(" le résultat"),
        vec![ThoughtEvent::ResponseChunk(" le résultat".into())]
    );
}

#[test]
fn xml_marker_split_across_deltas_is_atomic() {
    let mut stream = ThoughtStream::new(true);
    assert!(stream.feed("Réflexion puis </thi").is_empty());
    assert_eq!(
        stream.feed("nking>Réponse"),
        vec![
            ThoughtEvent::ThoughtChunk("Réflexion puis ".into()),
            ThoughtEvent::ThoughtEnd,
            ThoughtEvent::ResponseChunk("Réponse".into()),
        ]
    );
}

#[test]
fn opening_marker_is_never_exposed_to_consumers() {
    let mut stream = ThoughtStream::new(true);
    assert!(stream.feed("<thinking>raisonnement").is_empty());
    assert_eq!(
        stream.finish(),
        vec![
            ThoughtEvent::ThoughtChunk("raisonnement".into()),
            ThoughtEvent::ThoughtEnd,
        ]
    );
}

#[test]
fn finish_is_idempotent() {
    let mut stream = ThoughtStream::new(true);
    stream.feed("pensée courte");
    assert_eq!(
        stream.finish(),
        vec![
            ThoughtEvent::ThoughtChunk("pensée courte".into()),
            ThoughtEvent::ThoughtEnd,
        ]
    );
    assert!(stream.finish().is_empty());
    assert_eq!(stream.phase(), ThoughtPhase::Completed);
}

#[test]
fn markdown_fallback_preserves_boundary_marker_in_response() {
    for marker in ["\n\n# Titre", "\n\n### Sous-titre", "\n\n#### Section"] {
        let mut stream = ThoughtStream::new(true);
        stream.feed("Pensée suffisamment longue pour être conservée");
        let events = stream.feed(marker);
        assert!(events.contains(&ThoughtEvent::ThoughtEnd));
        assert!(events.contains(&ThoughtEvent::ResponseChunk(marker.into())));
    }
}

#[test]
fn bare_double_newline_does_not_end_thought() {
    let mut stream = ThoughtStream::new(true);
    stream.feed("Pensée suffisamment longue pour dépasser la fenêtre");
    let events = stream.feed("\n\nSuite de la pensée");
    assert!(!events.iter().any(|event| matches!(event, ThoughtEvent::ThoughtEnd)));
    assert!(!events.iter().any(|event| matches!(event, ThoughtEvent::ResponseChunk(_))));
    let tail = stream.finish();
    assert!(tail.iter().any(|event| match event {
        ThoughtEvent::ThoughtChunk(text) => text.contains("Suite de la pensée"),
        _ => false,
    }));
}

#[test]
fn legacy_splitter_facade_preserves_existing_contract() {
    let mut splitter = ThoughtSplitter::new(true);
    let (thought, message) = splitter.feed("Réflexion suffisamment longue pour être émise");
    assert!(!thought.is_empty());
    assert!(message.is_empty());

    let (thought, message) = splitter.feed("\n\n## Réponse\nVoici");
    assert_eq!(thought, "");
    assert_eq!(message, "\n\n## Réponse\nVoici");
    assert!(splitter.has_emitted_thought());
}
