use super::*;

#[test]
fn resolution_normale() { let r = resolve("gemini-3.6-flash", DEFAULT_MODEL).unwrap(); assert_eq!(r, Resolved { name: "gemini-3.6-flash".into(), mode: 1, think: 4, extra: None }); }
#[test]
fn extra_pro_enhanced() { let r = resolve("gemini-3.1-pro-enhanced", DEFAULT_MODEL).unwrap(); assert_eq!(r.mode, 3); assert_eq!(r.extra, Some(vec![(31, 2), (80, 3)])); }
#[test]
fn override_think() { let r = resolve("gemini-3.6-flash@think=0", DEFAULT_MODEL).unwrap(); assert_eq!(r.think, 0); let r = resolve("gemini-3.5-flash-thinking-lite@think=2", DEFAULT_MODEL).unwrap(); assert_eq!(r.think, 2); assert_eq!(r.mode, 5); }
#[test]
fn refuse_multiple_think_suffixes() { let err = resolve("gemini-3.6-flash@think=2@think=3", DEFAULT_MODEL).unwrap_err(); assert!(err.contains("Multiple @think="), "got: {err}"); }
#[test]
fn is_thinking_mode_justesse() { assert!(!is_thinking_mode(1)); assert!(is_thinking_mode(2)); assert!(!is_thinking_mode(3)); assert!(!is_thinking_mode(4)); assert!(is_thinking_mode(5)); assert!(!is_thinking_mode(6)); }
#[test]
fn repli_clé_inconnue() { let r = resolve("gpt-4o", DEFAULT_MODEL).unwrap(); assert_eq!(r.name, DEFAULT_MODEL); assert_eq!(r.mode, 1); }
#[test]
fn think_invalide() { let err = resolve("gemini-3.6-flash@think=abc", DEFAULT_MODEL).unwrap_err(); assert!(err.contains("Invalid think level: abc")); }
