//! Parsing de la réponse `StreamGenerate` (cf. spec §4.3 — vérité =
//! `vendor/gemini-web2api/gemini.py`).
//!
//! Corps : `)]}'` puis un flux de lignes JSON ; chaque ligne utile contient
//! `"wrb.fr"` et porte le texte **cumulé** dans `arr[0][2]` → `inner[4]`
//! (candidats) → `part[1]` (segments concaténables).

use anyhow::{bail, Result};
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

fn code_ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?s)```(?:python|javascript|text)\?code_(?:reference|stdout)&code_event_index=\d+\n.*?```\n?",
        )
        .expect("regex code_ref")
    })
}

fn card_content_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"http://googleusercontent\.com/card_content/\d+\n?").expect("regex card")
    })
}

/// Retire les blocs `code_reference`/`code_stdout` et les URL `card_content`
/// injectées par le backend dans les réponses de codage.
pub fn clean_text(text: &str, strip: bool) -> String {
    let out = code_ref_re().replace_all(text, "");
    let out = card_content_re().replace_all(&out, "").into_owned();
    if strip {
        out.trim().to_string()
    } else {
        out
    }
}

/// Erreur amont balisée `BardErrorInfo [n]` (ex. cookies expirés).
pub fn bard_error(raw: &str) -> Option<i64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"BardErrorInfo\s*\[(\d+)\]").expect("regex bard"));
    re.captures(raw)?.get(1)?.as_str().parse().ok()
}

/// Textes **cumulés par candidat** d'une ligne `wrb.fr` (concaténation des
/// segments de chaque candidat). Le plus long/non vide est le texte courant
/// (cf. spec §4.3 : candidats multiples → prendre le plus long).
fn candidate_texts(line: &str) -> Vec<String> {
    if !line.contains("\"wrb.fr\"") || line.len() < 200 {
        return Vec::new();
    }
    let Ok(arr) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let Some(inner_str) = arr.get(0).and_then(|a| a.get(2)).and_then(Value::as_str) else {
        return Vec::new();
    };
    if inner_str.len() < 50 {
        return Vec::new();
    }
    let Ok(inner) = serde_json::from_str::<Value>(inner_str) else {
        return Vec::new();
    };
    let Some(candidates) = inner.get(4).and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for part in candidates {
        let Some(segments) = part.get(1).and_then(Value::as_array) else {
            continue;
        };
        let text: String = segments.iter().filter_map(Value::as_str).collect();
        if !text.is_empty() {
            out.push(text);
        }
    }
    out
}

/// Détecte un blocage par la politique de sécurité de Gemini dans le flux.
///
/// Google Gemini peut renvoyer un flux « vide » (aucun candidat textuel) ou un
/// flux contenant un indicateur de blocage (clé `blockReason` dans la réponse
/// amont, texte « I can't help with that », etc.). Cette fonction inspecte
/// les données brutes accumulées par le `StreamDecoder` pour détecter ces cas.
///
/// Retourne `Some(reason)` si un blocage est détecté, `None` sinon.
pub fn detect_safety_block(raw: &str) -> Option<String> {
    // 1) Clé `blockReason` dans le JSON amont (format Google).
    if raw.contains("blockReason") {
        let start = raw
            .find(r#""blockReason":"#)
            .or_else(|| raw.find(r#""blockReason": "#));
        if let Some(start) = start {
            let after_colon = &raw[start..];
            let colon_pos = after_colon.find(':').unwrap_or(start + 15);
            let rest = after_colon[colon_pos + 1..].trim_start();
            if let Some(end) = rest.find('"') {
                let reason = &rest[..end];
                // Valeurs connues : SAFETY, OTHER, BLOCK_REASON_UNSPECIFIED.
                if !reason.is_empty() {
                    return Some(format!(
                        "Gemini a refusé de répondre (blockReason: {}). \
                         Reformulez votre prompt en évitant le contenu sensible.",
                        reason
                    ));
                }
            }
        }
        // Présent mais pas parsable — blocage générique.
        return Some(
            "Gemini a refusé de répondre (politique de sécurité). \
             Reformulez votre prompt."
                .to_string(),
        );
    }

    // 2) Indicateurs textuels courants de refus dans le flux cumulé.
    let safety_phrases = [
        "I can't help with that",
        "I'm not able to help with that",
        "I cannot fulfill this request",
        "I won't be able to help",
        "content safety",
        "against my safety guidelines",
        "violates safety policy",
    ];
    let lower = raw.to_lowercase();
    for phrase in &safety_phrases {
        if lower.contains(&phrase.to_lowercase()) {
            return Some(
                "Gemini a refusé de répondre à ce prompt (politique de contenu). \
                 Reformulez votre demande."
                    .to_string(),
            );
        }
    }

    None
}

/// Vérifie si le flux est terminé mais aucun contenu textuel n'a été produit.
///
/// Gemini peut fermer le flux proprement sans erreur HTTP ni `BardErrorInfo`,
/// mais sans aucun candidat textuel — ce qui signifie un refus silencieux.
pub fn is_empty_stream(raw: &str) -> bool {
    // Si on a des données mais aucun candidat n'a été extrait → refus silencieux.
    if raw.contains("\"wrb.fr\"") {
        let texts = candidate_texts(raw);
        if texts.is_empty() && raw.len() > 500 {
            return true;
        }
    }
    false
}

/// Texte final (mode non-streaming) : le plus long candidat nettoyé.
/// Erreur si le corps porte un `BardErrorInfo`.
pub fn final_text(raw: &str) -> Result<String> {
    if let Some(code) = bard_error(raw) {
        bail!("Gemini upstream rejected request: BardErrorInfo [{code}]");
    }
    let longest = raw
        .lines()
        .flat_map(candidate_texts)
        .max_by_key(String::len);
    Ok(clean_text(longest.as_deref().unwrap_or(""), true))
}

/// Décodeur ligne-à-ligne d'un flux HTTP : garde le reste d'une ligne
/// partielle entre deux `feed` et rend les textes des lignes complètes.
#[derive(Debug, Default)]
pub struct StreamDecoder {
    buf: String,
}

/// Taille maximale du buffer interne (I24). Un serveur malveillant ou un bug
/// amont pourrait envoyer un flux sans `\n` et faire croître `buf` sans fin.
/// On borne à 64 Mio (largement au-dessus d'une ligne `wrb.fr` typique qui
/// fait quelques Kio, mais assez bas pour éviter une OOM sur le worker).
const MAX_BUFFER_BYTES: usize = 64 * 1024 * 1024;

impl StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingère un chunk ; retourne les textes cumulés des lignes terminées.
    ///
    /// Garde-fou (I24) : si le buffer dépasse `MAX_BUFFER_BYTES` sans avoir
    /// rencontré de `\n`, on vide le buffer et on logge un avertissement —
    /// un flux Gemini légitime ne produit jamais une ligne aussi longue.
    pub fn feed(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(chunk);
        if self.buf.len() > MAX_BUFFER_BYTES && !self.buf.contains('\n') {
            tracing::warn!(
                "StreamDecoder: buffer non borné atteint {} octets sans newline — purge (flux amont buggé ou hostile ?)",
                self.buf.len()
            );
            self.buf.clear();
            return Vec::new();
        }
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find('\n') {
            let line = self.buf.split_off(pos + 1);
            let line = std::mem::replace(&mut self.buf, line);
            out.extend(candidate_texts(line.trim_end_matches('\n')));
        }
        out
    }

    /// Queue de ligne incomplète (utilisée pour détecter `BardErrorInfo`).
    pub fn pending(&self) -> &str {
        &self.buf
    }

    /// Tronque le buffer restant (appelé au changement de tentative).
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_enleve_references_et_cards() {
        let input = "avant\n```python?code_reference&code_event_index=12\nligne 1\nligne 2\n```\n"
            .to_string()
            + "milieu\nhttp://googleusercontent.com/card_content/7\nfin\n";
        assert_eq!(clean_text(&input, true), "avant\nmilieu\nfin");
    }

    /// Ligne représentative du câble (forme `["wrb.fr", <frame>, "<inner>"]`
    /// lue en `arr[0][2]` par le vendor) avec candidats à `inner[4]` et
    /// remplissage pour respecter le seuil des ≥ 200 caractères.
    fn wire_line(inner: &str) -> String {
        // Le JSON interne est embarqué **échappé** dans la chaîne extérieure.
        let escaped = serde_json::to_string(inner).expect("sérialisation JSON infaillible");
        format!("[[\"wrb.fr\",[62,0],{escaped}],[\"di\",72]]")
    }

    fn line_with_candidates(candidates: serde_json::Value) -> String {
        let inner = serde_json::json!([
            null,                                                      // 0
            ["tok"],                                                   // 1
            "padding-padding-padding-padding-padding-padding-padding-padding-padding-padding-padding-padding-padding-padding-padding", // 2 (jamais lu)
            [],                                                        // 3
            candidates,                                                // 4 ← candidats
            [],                                                        // 5
            [],                                                        // 6
            []                                                         // 7
        ]);
        wire_line(&inner.to_string())
    }

    #[test]
    fn candidate_texts_ligne_capturee() {
        let line = line_with_candidates(serde_json::json!([
            ["rcid-court", ["Bonjour"]],
            ["rcid-long", ["Bonjour, ", "le monde", " final"]]
        ]));
        assert!(line.len() >= 200, "ligne trop courte pour être examinée");
        // Deux candidats : texte cumulé par candidat.
        assert_eq!(
            candidate_texts(&line),
            vec!["Bonjour", "Bonjour, le monde final"]
        );
    }

    #[test]
    fn extract_ignore_lignes_inutiles() {
        assert!(candidate_texts(")]}'").is_empty());
        assert!(candidate_texts(&"x".repeat(250)).is_empty());
        assert!(candidate_texts(&format!("\"wrb.fr\"{}", "x".repeat(250))).is_empty());
    }

    #[test]
    fn final_text_longest_et_nettoye() {
        let raw = format!(
            ")]}}'\n{}\n\n",
            line_with_candidates(serde_json::json!([[
                "rcid",
                [
                    "court",
                    "```text?code_reference&code_event_index=3\ncode\n```"
                ]
            ]]))
        );
        assert_eq!(final_text(&raw).unwrap(), "court");
    }

    #[test]
    fn final_text_bard_error() {
        let raw = ")]}' foo\nBardErrorInfo [123] bar";
        assert!(final_text(raw)
            .unwrap_err()
            .to_string()
            .contains("BardErrorInfo [123]"));
    }

    #[test]
    fn stream_decoder_lignes_partiellepuis_complete() {
        let line = line_with_candidates(serde_json::json!([["rcid", ["abc"]]]));
        let mut dec = StreamDecoder::new();
        // Chunk coupé en deux au milieu de la ligne.
        let (a, b) = line.split_at(line.len() / 2);
        assert!(dec.feed(a).is_empty());
        let texts = dec.feed(&format!("{b}\n)]}}'"));
        assert_eq!(texts, vec!["abc"]);
    }
}
