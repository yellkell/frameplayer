//! Client for `steamos-devkit-service`, the on-device half of Valve's devkit
//! pairing flow (ValveSoftware/steamos-devkit).
//!
//! What we rely on, from the Steam Deck implementation:
//! * `GET  /properties.json` → JSON object mirroring the mDNS TXT record
//!   (`txtvers`, `login`, `settings`, `devkit1`, …).
//! * `GET  /login-name` → the account to SSH in as (plain text).
//! * `POST /register` with the OpenSSH public key line as the body. The
//!   device asks the user to approve; on approval the key is appended to
//!   `~/.ssh/authorized_keys`.
//!
//! [verify] All of the above on the Frame: endpoint names, whether
//! `/register` blocks until the user answers or returns immediately (we
//! handle both by polling SSH afterwards), the status code for "denied", and
//! the login name (`deck` on the Deck; the Frame may use another account).

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::time::Duration;
use url::Url;

/// Properties advertised by the devkit service (HTTP or mDNS TXT).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DevkitProperties {
    pub raw: Map<String, Value>,
}

impl DevkitProperties {
    /// Parse `/properties.json`. Values may be strings, numbers or arrays.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let v: Value = serde_json::from_slice(bytes).context("properties.json is not JSON")?;
        match v {
            Value::Object(raw) => Ok(Self { raw }),
            _ => bail!("properties.json is not a JSON object"),
        }
    }

    /// Build from mDNS TXT key/value pairs.
    pub fn from_txt(txt: HashMap<String, String>) -> Self {
        Self {
            raw: txt
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        }
    }

    fn str_field(&self, key: &str) -> Option<String> {
        match self.raw.get(key)? {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }

    /// SSH login name.
    pub fn login(&self) -> Option<String> {
        self.str_field("login")
    }

    pub fn txt_version(&self) -> Option<u32> {
        self.str_field("txtvers")?.parse().ok()
    }

    /// `settings` is a JSON document, sometimes embedded as a string.
    pub fn settings(&self) -> Option<Map<String, Value>> {
        match self.raw.get("settings")? {
            Value::Object(m) => Some(m.clone()),
            Value::String(s) => serde_json::from_str::<Value>(s).ok()?.as_object().cloned(),
            _ => None,
        }
    }

    /// Devkit protocol versions the service supports (`devkit1` key present ⇒ v1).
    pub fn supports_devkit1(&self) -> bool {
        self.raw.contains_key("devkit1")
    }
}

/// Result of `POST /register`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// Service accepted the request (the user may still need to approve).
    Accepted(String),
    /// The user (or policy) refused.
    Denied(String),
}

/// HTTP client for one headset's devkit service.
#[derive(Debug, Clone)]
pub struct DevkitClient {
    base: Url,
    http: reqwest::Client,
}

/// Base URL for a host and port (IPv6 literals bracketed).
pub fn service_url(host: &str, port: u16) -> Result<Url> {
    let h = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Url::parse(&format!("http://{h}:{port}/")).with_context(|| format!("bad host {host:?}"))
}

impl DevkitClient {
    pub fn new(host: &str, port: u16) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("frameplayer-install/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            base: service_url(host, port)?,
            http,
        })
    }

    pub fn base(&self) -> &Url {
        &self.base
    }

    pub async fn properties(&self) -> Result<DevkitProperties> {
        let url = self.base.join("properties.json")?;
        let resp = self
            .http
            .get(url.clone())
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .with_context(|| {
                format!("cannot reach the devkit service at {url}; is Developer Mode on?")
            })?;
        if !resp.status().is_success() {
            bail!("{url} returned {}", resp.status());
        }
        DevkitProperties::parse(&resp.bytes().await?)
    }

    pub async fn login_name(&self) -> Result<String> {
        let url = self.base.join("login-name")?;
        let resp = self
            .http
            .get(url.clone())
            .timeout(Duration::from_secs(10))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("{url} returned {}", resp.status());
        }
        let name = resp.text().await?.trim().to_string();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            bail!("devkit service returned an invalid login name {name:?}");
        }
        Ok(name)
    }

    /// Send our public key. `timeout` should cover the user walking over to
    /// the headset and approving.
    pub async fn register(
        &self,
        public_key_line: &str,
        timeout: Duration,
    ) -> Result<RegisterOutcome> {
        let url = self.base.join("register")?;
        let resp = self
            .http
            .post(url.clone())
            .timeout(timeout)
            .header(reqwest::header::CONTENT_TYPE, "text/plain")
            .body(format!("{}\n", public_key_line.trim()))
            .send()
            .await
            .with_context(|| format!("POST {url} failed"))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default().trim().to_string();
        classify_register(status.as_u16(), body)
    }
}

/// Map the register response status to an outcome.
pub fn classify_register(status: u16, body: String) -> Result<RegisterOutcome> {
    match status {
        200..=299 => Ok(RegisterOutcome::Accepted(body)),
        401 | 403 => Ok(RegisterOutcome::Denied(body)),
        s => bail!("devkit service rejected registration with HTTP {s}: {body}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_deck_style_properties() {
        let p = DevkitProperties::parse(
            br#"{"txtvers": 1, "login": "steamos", "settings": "{\"steam_play_debug\": 0}", "devkit1": ["devkit-1"]}"#,
        )
        .unwrap();
        assert_eq!(p.login().as_deref(), Some("steamos"));
        assert_eq!(p.txt_version(), Some(1));
        assert!(p.supports_devkit1());
        assert_eq!(p.settings().unwrap()["steam_play_debug"], 0);
    }

    #[test]
    fn tolerates_missing_and_object_settings() {
        let p = DevkitProperties::parse(br#"{"settings": {"a": true}, "login": ""}"#).unwrap();
        assert!(p.login().is_none());
        assert!(!p.supports_devkit1());
        assert_eq!(p.settings().unwrap()["a"], true);
        assert!(DevkitProperties::parse(b"[1,2]").is_err());
        assert!(DevkitProperties::parse(b"nope").is_err());
    }

    #[test]
    fn from_txt_record() {
        let mut m = HashMap::new();
        m.insert("login".to_string(), "deck".to_string());
        m.insert("txtvers".to_string(), "1".to_string());
        let p = DevkitProperties::from_txt(m);
        assert_eq!(p.login().as_deref(), Some("deck"));
        assert_eq!(p.txt_version(), Some(1));
    }

    #[test]
    fn register_status_mapping() {
        assert_eq!(
            classify_register(200, "ok".into()).unwrap(),
            RegisterOutcome::Accepted("ok".into())
        );
        assert!(matches!(
            classify_register(403, String::new()).unwrap(),
            RegisterOutcome::Denied(_)
        ));
        assert!(classify_register(500, "boom".into()).is_err());
    }

    #[test]
    fn urls() {
        assert_eq!(
            service_url("10.0.0.5", 32000).unwrap().as_str(),
            "http://10.0.0.5:32000/"
        );
        assert_eq!(
            service_url("fe80::1", 32000).unwrap().as_str(),
            "http://[fe80::1]:32000/"
        );
        assert_eq!(
            service_url("frame.local", 1).unwrap().as_str(),
            "http://frame.local:1/"
        );
    }
}
