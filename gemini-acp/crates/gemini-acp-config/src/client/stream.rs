//! Streaming HTTP : retry avec backoff exponentiel, lecture du flux Gemini,
//! construction de la requête, gestion des cookies et jetons de page.

use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use futures_util::StreamExt;
use crate::core::auth::sapisid_hash;
use crate::core::cookies::CookieJar;
use crate::core::frames::{self, StreamDecoder};
use crate::core::models;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, ORIGIN, REFERER};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use super::config::{ENDPOINT, TOKEN_TTL};
use super::payload::{
    emit_delta, extract_page_tokens, load_jar, next_reqid, payload,
};
use super::Client;

impl Client {
    // ---- cycle de vie ---------------------------------------------------

    /// Corps complet du tour (retry + lecture flux).
    pub(crate) async fn run_turn(
        &self,
        tx: mpsc::Sender<super::StreamItem>,
        prompt: String,
        refs: Vec<String>,
        resolved: &models::Resolved,
    ) -> anyhow::Result<()> {
        let attempts = self.inner.config.retry_attempts.max(1);
        let mut emitted = String::new();
        let mut decoder = StreamDecoder::new();

        for attempt in 1..=attempts {
            match self
                .attempt_http(&prompt, &refs, resolved, &mut decoder, &mut emitted, &tx)
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) => {
                    let es = e.to_string();
                    if es.contains("cookie") || es.contains("Cookie") || es.contains("BardErrorInfo") {
                        return Err(e);
                    }
                    if emitted.is_empty() && attempt < attempts {
                        // Backoff exponentiel avec jitter.
                        let base_ms = self.inner.config.retry_delay.as_millis() as u64;
                        let delay_ms = std::cmp::min(
                            base_ms * (1u64 << (attempt - 1)),
                            30_000,
                        );
                        let jitter = delay_ms / 4;
                        let ts_nanos = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos() as u64;
                        let jitter_ms = (ts_nanos % (2 * jitter + 1)).saturating_sub(jitter);
                        let effective = delay_ms.saturating_add(jitter_ms);
                        debug!(
                            tentative = attempt,
                            total = attempts,
                            "tentative échouée, retry dans {}ms — {e:#}",
                            effective
                        );
                        decoder.clear();
                        tokio::time::sleep(Duration::from_millis(effective)).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
        unreachable!("run_turn: la boucle de tentatives doit toujours retourner")
    }

    /// Une tentative HTTP : envoi + boucle de lecture/delta.
    async fn attempt_http(
        &self,
        prompt: &str,
        refs: &[String],
        resolved: &models::Resolved,
        decoder: &mut StreamDecoder,
        emitted: &mut String,
        tx: &mpsc::Sender<super::StreamItem>,
    ) -> anyhow::Result<Option<()>> {
        let (url, headers, body) = self.build_request(prompt, refs, resolved).await?;
        let response = self
            .inner
            .http
            .post(&url)
            .headers(headers)
            .body(body)
            .send()
            .await
            .context("envoi requête Gemini")?;
        let response = response.error_for_status().context("HTTP Gemini")?;
        let mut bytes_stream = response.bytes_stream();

        let mut raw_accumulator = String::new();
        const MAX_RAW_ACCUMULATOR: usize = 64 * 1024;

        loop {
            tokio::select! {
                _ = tx.closed() => return Ok(None),
                chunk = bytes_stream.next() => {
                    let Some(chunk) = chunk else {
                        // Fin du flux : vérifier le blocage sécurité.
                        if let Some(reason) = frames::detect_safety_block(&raw_accumulator) {
                            let _ = tx.send(Err(
                                crate::core::errors::GeminiError::SafetyBlocked(reason).to_string()
                            )).await;
                            return Ok(Some(()));
                        }
                        if emitted.is_empty() && frames::is_empty_stream(&raw_accumulator) {
                            let _ = tx.send(Err(
                                crate::core::errors::GeminiError::SafetyBlocked(
                                    "Gemini n'a produit aucune réponse (refus silencieux). \
                                     Reformulez votre prompt.".to_string()
                                ).to_string()
                            )).await;
                            return Ok(Some(()));
                        }
                        return Ok(Some(()))
                    };
                    let bytes = chunk.context("lecture flux Gemini")?;
                    let text = String::from_utf8_lossy(&bytes);
                    trace!("chunk {} octets, queue ligne {}", text.len(), decoder.pending().len());

                    if raw_accumulator.len() < MAX_RAW_ACCUMULATOR {
                        raw_accumulator.push_str(&text);
                        if raw_accumulator.len() > MAX_RAW_ACCUMULATOR {
                            raw_accumulator.truncate(MAX_RAW_ACCUMULATOR);
                        }
                    }

                    let combined = format!("{}{}", decoder.pending(), text);
                    if combined.contains("BardErrorInfo") {
                        let code = frames::bard_error(&combined).unwrap_or(0);
                        bail!("Gemini upstream rejected request: BardErrorInfo [{code}]");
                    }

                    if let Some(reason) = frames::detect_safety_block(&combined) {
                        let _ = tx.send(Err(
                            crate::core::errors::GeminiError::SafetyBlocked(reason).to_string()
                        )).await;
                        return Ok(Some(()));
                    }

                    for candidate in decoder.feed(&text) {
                        emit_delta(candidate, emitted, tx).await?;
                    }
                }
            }
        }
    }

    // ---- construction requête --------------------------------------------

    async fn build_request(
        &self,
        prompt: &str,
        refs: &[String],
        resolved: &models::Resolved,
    ) -> anyhow::Result<(String, HeaderMap, String)> {
        let inner = &self.inner;
        let prefix = inner
            .config
            .auth_user
            .map(|n| format!("/u/{n}"))
            .unwrap_or_default();
        let reqid = next_reqid();
        let url = format!(
            "https://gemini.google.com{prefix}/{ENDPOINT}?bl={}&hl=en&_reqid={reqid}&rt=c",
            inner.config.bl
        );

        let jar = self.jar().await;
        let mut headers = HeaderMap::new();
        if let Some(cookie) = jar.as_ref().and_then(CookieJar::header) {
            let mut v = HeaderValue::from_str(&cookie).context("header Cookie invalide")?;
            v.set_sensitive(true);
            headers.insert(reqwest::header::COOKIE, v);
        }
        if let Some(sapisid) = jar.as_ref().and_then(CookieJar::sapisid) {
            let auth = sapisid_hash(sapisid, "https://gemini.google.com");
            let mut v = HeaderValue::from_str(&auth).context("header Authorization invalide")?;
            v.set_sensitive(true);
            headers.insert(AUTHORIZATION, v);
        }
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/x-www-form-urlencoded;charset=utf-8"),
        );
        headers.insert(
            ORIGIN,
            HeaderValue::from_static("https://gemini.google.com"),
        );
        headers.insert(
            REFERER,
            HeaderValue::from_str(&format!("https://gemini.google.com{prefix}/app"))?,
        );
        headers.insert("X-Same-Domain", HeaderValue::from_static("1"));
        if let Some(user) = inner.config.auth_user {
            headers.insert("X-Goog-AuthUser", HeaderValue::from_str(&user.to_string())?);
        }

        let body = payload(
            prompt,
            resolved,
            refs,
            self.page_tokens().await.at.as_deref(),
        );
        Ok((url, headers, body))
    }

    // ---- gestion cookies / jetons ---------------------------------------

    /// Recharge les cookies si le fichier a changé (mtime).
    pub(crate) async fn jar(&self) -> Option<CookieJar> {
        let mut guard = self.inner.jar.write().await;
        let mtime = tokio::fs::metadata(&self.inner.config.cookie_file)
            .await
            .and_then(|m| m.modified())
            .ok();
        if guard.1 != mtime {
            *guard = load_jar(&self.inner.config.cookie_file).await;
        }
        guard.0.clone()
    }

    /// Jetons de page `/app` : cache ~10 min, rafraîchis best-effort.
    pub(crate) async fn page_tokens(&self) -> super::config::PageTokens {
        {
            let guard = self.inner.page.read().await;
            if let Some((tokens, at)) = guard.as_ref() {
                if at.elapsed() < TOKEN_TTL {
                    return tokens.clone();
                }
            }
        }
        self.refresh_page().await;
        self.inner
            .page
            .read()
            .await
            .as_ref()
            .map(|(t, _)| t.clone())
            .unwrap_or_default()
    }

    pub(crate) async fn refresh_page(&self) {
        let prefix = self
            .inner
            .config
            .auth_user
            .map(|n| format!("/u/{n}"))
            .unwrap_or_default();
        let url = format!("https://gemini.google.com{prefix}/app");
        match self.inner.http.get(&url).send().await {
            Ok(resp) => {
                let body = match resp.text().await {
                    Ok(b) => b,
                    Err(e) => {
                        warn!("lecture /app impossible: {e:#}");
                        return;
                    }
                };
                let tokens = extract_page_tokens(&body);
                *self.inner.page.write().await = Some((tokens.clone(), Instant::now()));
                debug!(
                    "jetons de page récupérés (at: {}, push_id: {}, pctx: {})",
                    tokens.at.is_some(),
                    tokens.push_id.is_some(),
                    tokens.pctx.is_some()
                );
            }
            Err(e) => {
                let safe = self.inner.config.proxy.as_ref().map(|_| "<redacted>");
                warn!("GET /app impossible: {e:#} proxy={:?}", safe);
            }
        }
    }
}
