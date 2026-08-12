//! Utilitaires temps centralisés (refactor M8 — cf. spec §4.3).
//!
//! Déduplique `now_unix()` (présent dans `cookies.rs`, `auth.rs`,
//! `gemini/client.rs`) et `now_iso()` (ré-implémenté manuellement dans
//! `acp/state.rs` avec 30 lignes d'arithmétique grégorienne).
//!
//! L'algorithme `civil_from_days` (H. Hinnant) est préservé pour éviter
//! la dépendance externe `time` ou `chrono` — le projet reste minimal.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds Unix courantes (UTC). Retourne 0 en cas d'horloge antérieure à l'epoch.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Variante `u64` (utilisé par `gemini_client::Client` pour `_reqid`).
pub fn now_unix_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Horodatage ISO 8601 UTC (secondes) — `2026-08-11T03:48:22Z`.
/// Algorithme *civil_from_days* (H. Hinnant) déterministe, sans dépendance externe.
pub fn now_iso() -> String {
    let secs = now_unix_u64();
    let days = secs / 86_400;
    let (h, m, s) = ((secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
    let (y, mo, da) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{da:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Conversion jours écoulés depuis 1970-01-01 → (année, mois, jour) grégorien.
/// Algorithme public de Howard Hinnant — déterministe et testé.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_epoch() {
        // 1970-01-01T00:00:00Z
        let (y, m, d) = civil_from_days(0);
        assert_eq!((y, m, d), (1970, 1, 1));
    }

    #[test]
    fn iso_date_recente() {
        // 2026-08-10T00:00:00Z (vérifié indépendamment en Python).
        let secs = 1_786_320_000u64;
        let (y, m, d) = civil_from_days((secs / 86_400) as i64);
        assert_eq!((y, m, d), (2026, 8, 10));
    }

    #[test]
    fn now_iso_format_valide() {
        let s = now_iso();
        assert_eq!(s.len(), 20, "format YYYY-MM-DDTHH:MM:SSZ");
        assert!(s.ends_with('Z'));
        assert_eq!(s.as_bytes()[4], b'-');
        assert_eq!(s.as_bytes()[10], b'T');
        assert_eq!(s.as_bytes()[19], b'Z');
    }

    #[test]
    fn now_unix_croissant() {
        let a = now_unix();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let b = now_unix();
        assert!(b >= a, "now_unix doit être monotone");
    }
}
