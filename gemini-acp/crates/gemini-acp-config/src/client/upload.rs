//! Upload Scotty resumable (port de `vendor/gemini-web2api/multimodal.py`).
//!
//! Décode l'image base64, initie l'upload, pousse les octets et retourne
//! la référence (`/generated/…`) à placer dans `inner[0][3]` du payload.

use anyhow::{bail, Context};
use base64::Engine;
use crate::core::auth::sapisid_hash;
use crate::core::cookies::CookieJar;
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use tracing::debug;

use super::config::{
    DEFAULT_PCTX, DEFAULT_PUSH_ID, MAX_IMAGE_B64, UPLOAD_ENDPOINT, UPLOAD_HOST,
};
use super::Client;

impl Client {
    /// Upload Scotty resumable : décode l'image base64, initie l'upload,
    /// pousse les octets et retourne la référence.
    pub async fn upload_image(&self, base64_data: &str, mime_type: &str) -> anyhow::Result<String> {
        // Éventuel préfixe `data:<mime>;base64,` (tolérant, comme Zed/API).
        let b64 = base64_data
            .strip_prefix("data:")
            .and_then(|s| s.split_once(','))
            .map(|(_, b)| b)
            .unwrap_or(base64_data);
        if b64.len() > MAX_IMAGE_B64 {
            bail!("image base64 trop volumineuse ({} octets)", b64.len());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .context("décodage base64 de l'image")?;
        if bytes.is_empty() {
            bail!("image vide");
        }

        let tokens = self.page_tokens().await;
        let push_id = tokens.push_id.as_deref().unwrap_or(DEFAULT_PUSH_ID);
        let pctx = tokens.pctx.as_deref().unwrap_or(DEFAULT_PCTX);
        let jar = self.jar().await;

        // 1) Initiation : `X-Goog-Upload-Command: start` → URL d'upload.
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("Push-ID", HeaderValue::from_str(push_id)?);
        h.insert("X-Tenant-Id", HeaderValue::from_static("bard-storage"));
        h.insert("X-Client-Pctx", HeaderValue::from_str(pctx)?);
        h.insert(
            "X-Goog-Upload-Header-Content-Length",
            HeaderValue::from_str(&bytes.len().to_string())?,
        );
        h.insert(
            "X-Goog-Upload-Header-Content-Type",
            HeaderValue::from_str(mime_type)?,
        );
        h.insert(
            "X-Goog-Upload-Protocol",
            HeaderValue::from_static("resumable"),
        );
        h.insert("X-Goog-Upload-Command", HeaderValue::from_static("start"));
        h.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded;charset=utf-8"),
        );
        if let Some(cookie) = jar.as_ref().and_then(CookieJar::header) {
            let mut v = HeaderValue::from_str(&cookie).context("header Cookie invalide")?;
            v.set_sensitive(true);
            h.insert(reqwest::header::COOKIE, v);
        }
        if let Some(sapisid) = jar.as_ref().and_then(CookieJar::sapisid) {
            let mut v = HeaderValue::from_str(&sapisid_hash(sapisid, "https://gemini.google.com"))
                .context("header Authorization invalide")?;
            v.set_sensitive(true);
            h.insert(AUTHORIZATION, v);
        }
        let resp = self
            .inner
            .http
            .post(UPLOAD_ENDPOINT)
            .headers(h)
            .send()
            .await
            .context("initiation upload Scotty")?;
        let upload_url = resp
            .headers()
            .get("x-goog-upload-url")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .context("réponse d'initiation sans X-Goog-Upload-URL")?;
        // Sécurité : valider que l'URL d'upload pointe bien vers l'hôte attendu.
        let upload_host = reqwest::Url::parse(&upload_url)
            .context("URL d'upload Scotty invalide")?
            .host_str()
            .unwrap_or("")
            .to_string();
        if upload_host != UPLOAD_HOST {
            bail!(
                "hôte d'upload Scotty inattendu: {upload_host} (attendu: {UPLOAD_HOST}) — possible MITM"
            );
        }

        // 2) Envoi des octets + finalisation.
        let mut h2 = reqwest::header::HeaderMap::new();
        h2.insert(
            "X-Goog-Upload-Command",
            HeaderValue::from_static("upload, finalize"),
        );
        h2.insert("X-Goog-Upload-Offset", HeaderValue::from_static("0"));
        h2.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        let resp = self
            .inner
            .http
            .post(&upload_url)
            .headers(h2)
            .body(bytes)
            .send()
            .await
            .context("envoi image Scotty")?;
        let file_ref = resp
            .text()
            .await
            .context("lecture référence Scotty")?
            .trim()
            .to_string();
        if !file_ref.starts_with('/') {
            bail!("référence de fichier invalide: {file_ref}");
        }
        debug!(r#ref = %file_ref, "image uploadée (Scotty)");
        Ok(file_ref)
    }
}
