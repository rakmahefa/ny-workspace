//! Chargement des cookies Google depuis 3 formats (cf. spec §4.1) :
//! export **EditThisCookie** (tableau JSON), objet `{"cookie", "sapisid"}`
//! (format gemini-web2api) et chaîne brute `"k=v; k2=v2"`.

use anyhow::{Context, Result};
use serde::Deserialize;

fn is_google_domain(domain: &str) -> bool {
    domain == "google.com" || domain.ends_with(".google.com")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: Option<String>,
    pub expires_at: Option<i64>,
}

impl Cookie {
    fn valid(&self, now: i64) -> bool {
        !self.name.is_empty()
            && !self.value.is_empty()
            && self.expires_at.is_none_or(|exp| exp > now)
            && self.domain.as_deref().is_none_or(is_google_domain)
    }
}

impl From<EtcCookie> for Cookie {
    fn from(c: EtcCookie) -> Self {
        Self {
            name: c.name,
            value: c.value,
            domain: c.domain,
            expires_at: c.expiration_date.map(|f| f as i64),
        }
    }
}

#[derive(Debug, Deserialize)]
struct EtcCookie {
    name: String,
    value: String,
    #[serde(default)]
    domain: Option<String>,
    #[serde(default, rename = "expirationDate")]
    expiration_date: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ObjectCookie {
    #[serde(default)]
    cookie: String,
    #[serde(default)]
    sapisid: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
    sapisid: Option<String>,
}

impl CookieJar {
    pub fn parse(input: &str) -> Result<Self> {
        match input.trim_start().chars().next() {
            Some('[') => Self::parse_etc(input),
            Some('{') => Self::parse_object(input),
            _ => Ok(Self::parse_raw(input)),
        }
    }

    fn parse_etc(input: &str) -> Result<Self> {
        let etc: Vec<EtcCookie> = serde_json::from_str(input)
            .context("cookie.json n'est pas un export EditThisCookie valide")?;
        Ok(Self { cookies: etc.into_iter().map(Cookie::from).collect(), sapisid: None })
    }

    fn parse_object(input: &str) -> Result<Self> {
        let obj: ObjectCookie = serde_json::from_str(input)
            .context("format objet cookie invalide (attendu {\"cookie\", \"sapisid\"})")?;
        Ok(Self { cookies: Self::parse_raw(&obj.cookie).cookies, sapisid: obj.sapisid })
    }

    fn parse_raw(input: &str) -> Self {
        let cookies = input
            .split(';')
            .filter_map(|pair| {
                let (name, value) = pair.trim().split_once('=')?;
                Some(Cookie { name: name.trim().to_string(), value: value.trim().to_string(), domain: None, expires_at: None })
            })
            .collect();
        Self { cookies, sapisid: None }
    }

    pub fn header(&self) -> Option<String> {
        self.header_at(now_unix())
    }

    fn header_at(&self, now: i64) -> Option<String> {
        let joined = self.cookies.iter().filter(|c| c.valid(now)).map(|c| format!("{}={}", c.name, c.value)).collect::<Vec<_>>().join("; ");
        (!joined.is_empty()).then_some(joined)
    }

    pub fn sapisid(&self) -> Option<&str> {
        self.sapisid.as_deref().or_else(|| self.cookies.iter().find(|c| c.name == "SAPISID").map(|c| c.value.as_str()))
    }
}

fn now_unix() -> i64 {
    crate::core::time::now_unix()
}

#[cfg(test)]
#[path = "../test/cookies.rs"]
mod tests;
