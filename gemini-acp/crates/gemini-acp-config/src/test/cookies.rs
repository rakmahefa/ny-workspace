use super::*;
const NOW: i64 = 1_800_000_000;
#[test]
fn edit_this_cookie_filtre_et_sapisid() { let json = r#"[{"name":"SAPISID","value":"abc","domain":".google.com","expirationDate":1900000000},{"name":"HSID","value":"def","domain":"gemini.google.com"},{"name":"expired","value":"z","domain":".google.com","expirationDate":1000000000},{"name":"wrong","value":"y","domain":".example.com"},{"name":"empty","value":"","domain":".google.com"}]"#; let jar = CookieJar::parse(json).unwrap(); assert_eq!(jar.header_at(NOW).unwrap(), "SAPISID=abc; HSID=def"); assert_eq!(jar.sapisid(), Some("abc")); assert!(jar.header_at(NOW).unwrap().contains("HSID=def")); }
#[test]
fn format_objet() { let jar = CookieJar::parse(r#"{"cookie":"SAPISID=abc; HSID=def","sapisid":"abc"}"#).unwrap(); assert_eq!(jar.header().unwrap(), "SAPISID=abc; HSID=def"); assert_eq!(jar.sapisid(), Some("abc")); }
#[test]
fn format_brut() { let jar = CookieJar::parse("SAPISID=s1; HSID=h2; empty=").unwrap(); assert_eq!(jar.header().unwrap(), "SAPISID=s1; HSID=h2"); assert_eq!(jar.sapisid(), Some("s1")); }
#[test]
fn formats_invalides() { assert!(CookieJar::parse("{pas du json").is_err()); assert!(CookieJar::parse("[pas du json").is_err()); }
#[test]
fn header_vide_si_tout_expire() { let jar = CookieJar::parse(r#"[{"name":"SAPISID","value":"a","domain":".google.com","expirationDate":1000000000}]"#).unwrap(); assert_eq!(jar.header_at(NOW), None); }
