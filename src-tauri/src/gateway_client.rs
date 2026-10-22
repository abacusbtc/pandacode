//! The gateway the app connects to: its address as a non-secret setting, its
//! bearer token in the OS secure store.
//!
//! Same shape as `cloud.rs`: settings that are not secret live as one JSON value
//! in the secure store, beside the secrets themselves. The token is keyed by the
//! URL, so switching gateways cannot leave an old token behind, and the app holds
//! exactly one connection at a time.

use serde::{Deserialize, Serialize};

use crate::secret_store;

const SERVICE: &str = "app.pandacode.gateway";
const SETTINGS: &str = "settings";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
struct Settings {
    url: Option<String>,
}

/// What the webview needs to open a client: the address and its token.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GatewayConfig {
    pub url: String,
    pub token: String,
}

fn settings() -> Settings {
    secret_store::get(SERVICE, SETTINGS).and_then(|json| serde_json::from_str(&json).ok()).unwrap_or_default()
}

fn save_settings(settings: &Settings) -> crate::Result<()> {
    secret_store::set(SERVICE, SETTINGS, &serde_json::to_string(settings).map_err(|error| error.to_string())?)
}

/// A gateway address is an HTTP server, so anything else is a typo rather than an
/// address to try. The trailing slash is dropped because paths are joined onto it.
pub fn normalize_url(url: &str) -> Result<String, String> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("Enter the gateway's address, like http://127.0.0.1:4175.".into());
    }
    if trimmed.len() > 2048 {
        return Err("That gateway address is too long.".into());
    }
    let host = trimmed.strip_prefix("https://").or_else(|| trimmed.strip_prefix("http://")).ok_or("The gateway address must start with http:// or https://.")?;
    if host.is_empty() || host.starts_with('/') {
        return Err("Enter the gateway's address, like http://127.0.0.1:4175.".into());
    }
    Ok(trimmed.to_owned())
}

/// The saved gateway, or `None` when there is no address or no token for it — the
/// boot path only switches to remote mode when it can actually connect.
pub fn config() -> Option<GatewayConfig> {
    let url = settings().url?;
    let token = secret_store::get(SERVICE, &url)?;
    Some(GatewayConfig { url, token })
}

/// Saves an address and its token, replacing any previous connection.
pub fn connect(url: &str, token: &str) -> Result<GatewayConfig, String> {
    let url = normalize_url(url)?;
    let token = token.trim();
    if token.is_empty() {
        return Err("Enter the gateway's token.".into());
    }
    // One connection at a time: an old URL's token is removed rather than left in
    // the keychain where nothing will ever read it again.
    if let Some(previous) = settings().url {
        if previous != url {
            secret_store::remove(SERVICE, &previous);
        }
    }
    secret_store::set(SERVICE, &url, token)?;
    save_settings(&Settings { url: Some(url.clone()) })?;
    Ok(GatewayConfig { url, token: token.to_owned() })
}

/// Forgets the gateway: both the token and the address.
pub fn disconnect() {
    if let Some(url) = settings().url {
        secret_store::remove(SERVICE, &url);
    }
    secret_store::remove(SERVICE, SETTINGS);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_must_be_http_and_loses_its_trailing_slash() {
        assert_eq!(normalize_url(" http://127.0.0.1:4175/ ").unwrap(), "http://127.0.0.1:4175");
        assert_eq!(normalize_url("https://gw.example").unwrap(), "https://gw.example");
        assert!(normalize_url("").is_err());
        assert!(normalize_url("   ").is_err());
        assert!(normalize_url("127.0.0.1:4175").is_err());
        assert!(normalize_url("ftp://gw.example").is_err());
        assert!(normalize_url("http://").is_err());
        assert!(normalize_url(&format!("http://{}", "a".repeat(2100))).is_err());
    }

    #[test]
    fn connect_then_config_round_trips_through_the_secure_store() {
        disconnect();
        assert_eq!(config(), None);
        let saved = connect("http://gw.test/", "secret-token").unwrap();
        assert_eq!(saved.url, "http://gw.test");
        assert_eq!(config(), Some(saved));
    }

    #[test]
    fn a_connection_without_a_token_is_not_config() {
        disconnect();
        save_settings(&Settings { url: Some("http://gw.test".into()) }).unwrap();
        assert_eq!(config(), None);
    }

    #[test]
    fn switching_gateways_drops_the_previous_token() {
        disconnect();
        connect("http://first.test", "one").unwrap();
        connect("http://second.test", "two").unwrap();
        assert_eq!(config().unwrap().url, "http://second.test");
        assert_eq!(secret_store::get(SERVICE, "http://first.test"), None);
    }

    #[test]
    fn disconnect_forgets_both_halves() {
        connect("http://gw.test", "secret-token").unwrap();
        disconnect();
        assert_eq!(config(), None);
        assert_eq!(secret_store::get(SERVICE, "http://gw.test"), None);
    }

    #[test]
    fn an_empty_token_is_refused() {
        disconnect();
        assert!(connect("http://gw.test", "   ").is_err());
        assert_eq!(config(), None);
    }
}
