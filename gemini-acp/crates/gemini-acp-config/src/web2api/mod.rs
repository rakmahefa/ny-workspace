//! Proxy HTTP compatible OpenAI/Google — anciennement le crate
//! `gemini-web2api`. Le binaire `gemini-web2api` (voir `main.rs`, câblé via
//! `[[bin]] path = "src/web2api/main.rs"`) possède son propre arbre de
//! modules ; ce fichier expose les mêmes modules à la bibliothèque pour une
//! éventuelle réutilisation (spec §3.2/§2.2).

pub mod chat;
pub mod config;
pub mod convert;
pub mod google;
pub mod http;
pub mod responses;
