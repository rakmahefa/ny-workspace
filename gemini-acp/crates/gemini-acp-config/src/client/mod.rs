//! Client web API Gemini (cf. spec §4.2/§4.3 — vérité =
//! `vendor/gemini-web2api/gemini.py`).
//!
//! Architecture modulaire :
//! - [`config`] : configuration, constantes, types internes (Config, PageTokens, ClientInner)
//! - [`payload`] : construction payload f.req, encodage URL, extraction jetons, helpers
//! - [`stream`] : streaming HTTP, retry, construction requête, gestion cookies/jetons
//! - [`upload`] : upload Scotty resumable (images)
//! - Ce fichier : struct `Client` (new/stream/complete), re-exports

mod config;
mod payload;
mod stream;
mod upload;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub use config::{ClientInner, Config, StreamItem, DEFAULT_BL};

/// Client partageable entre sessions (reqwest interne unique, rechargement
/// automatique des cookies quand le fichier change).
#[derive(Clone)]
pub struct Client {
    pub(crate) inner: Arc<ClientInner>,
}

impl Client {
    /// Charge les cookies (3 formats), prépare le client HTTP, récupère le
    /// token `at` best-effort.
    pub async fn new(config: Config) -> Result<Self> {
        use config::USER_AGENT;

        let mut builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(config.request_timeout);
        if let Some(proxy) = &config.proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy).context("proxy invalide")?);
        }
        let jar = payload::load_jar(&config.cookie_file).await;
        match &jar.0 {
            Some(cookies) => {
                let n = cookies.header().map_or(0, |h| h.split(';').count());
                debug!(
                    "cookies chargés: {} paires, SAPISID {}",
                    n,
                    if cookies.sapisid().is_some() {
                        "présent"
                    } else {
                        "absent"
                    }
                );
            }
            None => warn!(
                "aucun cookie chargé depuis {:?} — les requêtes échoueront",
                config.cookie_file
            ),
        }
        let inner = Arc::new(ClientInner {
            http: builder.build().context("construction client HTTP")?,
            config,
            jar: tokio::sync::RwLock::new(jar),
            page: tokio::sync::RwLock::new(None),
        });
        let client = Self { inner };
        client.refresh_page().await;
        Ok(client)
    }

    /// Démarre un tour : envoie `prompt` au modèle `model` (suffixe
    /// `@think=N` ou `think` explicite) et retourne le canal des deltas.
    /// `refs` = références de fichiers (upload Scotty) ;
    /// vide = pas d'image. Le drop du `Receiver` annule la requête en vol.
    pub async fn stream(
        &self,
        prompt: &str,
        model: &str,
        think: Option<u32>,
        refs: &[String],
    ) -> Result<mpsc::Receiver<StreamItem>> {
        let model_arg = match think {
            Some(t) => format!("{model}@think={t}"),
            None => model.to_string(),
        };
        let resolved = crate::core::models::resolve(&model_arg, &self.inner.config.default_model)
            .map_err(|e| anyhow::anyhow!(e))?;
        debug!(
            "stream: {} -> mode {} think {} extra {:?}",
            resolved.name, resolved.mode, resolved.think, resolved.extra
        );

        let (tx, rx) = mpsc::channel(16);
        let client = self.clone();
        let prompt = prompt.to_string();
        let refs = refs.to_vec();
        tokio::spawn(async move {
            if let Err(e) = client.run_turn(tx.clone(), prompt, refs, &resolved).await {
                let _ = tx.send(Err(format!("{e:#}"))).await;
            }
        });
        Ok(rx)
    }

    /// Corps complet du tour (agrège les deltas) — pratique pour les tests.
    pub async fn complete(
        &self,
        prompt: &str,
        model: &str,
        think: Option<u32>,
        refs: &[String],
    ) -> Result<String> {
        let mut rx = self.stream(prompt, model, think, refs).await?;
        let mut out = String::new();
        while let Some(item) = rx.recv().await {
            match item {
                Ok(delta) => out.push_str(&delta),
                Err(e) => anyhow::bail!("{e}"),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models;
    use payload::decode_freq;

    #[test]
    fn payload_102_cases() {
        let resolved = models::Resolved {
            name: "m".into(),
            mode: 1,
            think: 4,
            extra: Some(vec![(31, 2)]),
        };
        let body = payload::payload("bonjour", &resolved, &[], Some("tok"));
        assert!(body.contains("f.req="));
        assert!(body.contains("&at=tok"));
        let decoded = decode_freq(&body);
        let outer: serde_json::Value = serde_json::from_str(&decoded).unwrap();
        let inner: serde_json::Value = serde_json::from_str(outer[1].as_str().unwrap()).unwrap();
        let arr = inner.as_array().unwrap();
        assert_eq!(arr.len(), 102);
        assert_eq!(arr[79], 1);
        assert_eq!(arr[17][0][0], 4);
        assert_eq!(arr[59].as_str().unwrap().len(), 36); // uuid hyphené
        assert_eq!(arr[31], 2);
        assert_eq!(arr[0][0], "bonjour");
        assert!(arr[0][3].is_null()); // pas de refs
    }

    #[test]
    fn payload_avec_refs_images() {
        let resolved = models::resolve("gemini-3.6-flash", models::DEFAULT_MODEL).unwrap();
        let refs = vec![
            "/generated/image1".to_string(),
            "/generated/image2".to_string(),
        ];
        let body = payload::payload("décris", &resolved, &refs, None);
        let outer: serde_json::Value = serde_json::from_str(&decode_freq(&body)).unwrap();
        let arr: serde_json::Value = serde_json::from_str(outer[1].as_str().unwrap()).unwrap();
        assert_eq!(
            arr[0][3],
            serde_json::json!([
                [null, null, "/generated/image1"],
                [null, null, "/generated/image2"]
            ])
        );
    }

    #[test]
    fn token_extraction() {
        let body = r#"<script>window.WIZ_global_data = {"SAPISID": "x", "SNlM0e":"AbCdEf123", "qKIAYe":"feeds/abc123", "Ylro7b":"CgcSXYZ"};</script>"#;
        let t = payload::extract_page_tokens(body);
        assert_eq!(t.at.unwrap(), "AbCdEf123");
        assert_eq!(t.push_id.unwrap(), "feeds/abc123");
        assert_eq!(t.pctx.unwrap(), "CgcSXYZ");
        assert!(payload::extract_page_tokens("rien ici").at.is_none());
    }

    #[test]
    fn encodage_form() {
        let params = vec![
            ("a b".to_string(), "x=y".to_string()),
            ("c".to_string(), "é".to_string()),
        ];
        assert_eq!(payload::form_urlencode(&params), "a+b=x%3Dy&c=%C3%A9");
    }

    #[test]
    fn emit_delta_sequence() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut emitted = String::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            payload::emit_delta("Bonj".to_string(), &mut emitted, &tx)
                .await
                .unwrap();
            payload::emit_delta("Bonjour".to_string(), &mut emitted, &tx)
                .await
                .unwrap();
            payload::emit_delta("Bonjour le".to_string(), &mut emitted, &tx)
                .await
                .unwrap();
            payload::emit_delta("Bonjour le".to_string(), &mut emitted, &tx)
                .await
                .unwrap(); // pas de nouveauté
            assert_eq!(rx.recv().await.unwrap().unwrap(), "Bonj");
            assert_eq!(rx.recv().await.unwrap().unwrap(), "our");
            assert_eq!(rx.recv().await.unwrap().unwrap(), " le");
            assert!(rx.try_recv().unwrap_err().to_string().contains("empty"));
        });
    }

    #[test]
    fn emit_delta_divergence() {
        let (tx, _rx) = mpsc::channel(8);
        let mut emitted = String::from("Bonjour tout le monde");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let err = payload::emit_delta("Autre chose".to_string(), &mut emitted, &tx)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("content changed"));
        });
    }
}
