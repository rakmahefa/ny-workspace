//! Auth des clients web Google : header `Authorization: SAPISIDHASH …`.
//!
//! `hash = SHA1(hex minuscule) de "{ts} {sapisid} {origin}"`, présenté comme
//! `SAPISIDHASH {ts}_{hash}` (cf. spec §4.1).

use crate::core::time::now_unix_u64;
use sha1::{Digest, Sha1};

/// Variante de `sapisid_hash` à horodatage fixé (tests / vecteurs connus).
pub fn sapisid_hash_at(sapisid: &str, origin: &str, ts: u64) -> String {
    let digest = {
        let mut hasher = Sha1::new();
        hasher.update(format!("{ts} {sapisid} {origin}"));
        hex::encode(hasher.finalize())
    };
    format!("SAPISIDHASH {ts}_{digest}")
}

/// En-tête `Authorization` pour `origin` (défaut `https://gemini.google.com`).
pub fn sapisid_hash(sapisid: &str, origin: &str) -> String {
    sapisid_hash_at(sapisid, origin, now_unix_u64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vecteur_connu() {
        // Vérifié indépendamment en Python (hashlib.sha1) le 2026-08-10.
        assert_eq!(
            sapisid_hash_at("test-sapisid", "https://gemini.google.com", 1_700_000_000),
            "SAPISIDHASH 1700000000_8b5814e54e0d42919ef58b02d65c57aa9e31a041"
        );
    }

    #[test]
    fn format_deux_parties() {
        let h = sapisid_hash("s", "https://gemini.google.com");
        let (head, tail) = h.split_once('_').unwrap();
        assert!(head.starts_with("SAPISIDHASH "));
        assert_eq!(tail.len(), 40); // sha1 hex
    }
}
