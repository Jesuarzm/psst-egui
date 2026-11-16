use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

use oauth2::PkceCodeVerifier;
use psst_core::{
    connection::Credentials,
    oauth,
    session::{SessionConfig, SessionConnection},
};

use crate::config;

const PORT: u16 = 8888;

pub fn load_credentials() -> Option<Credentials> {
    serde_json::from_value(config::read().get("credentials")?.clone()).ok()
}

pub fn has_credentials() -> bool {
    load_credentials().is_some()
}

pub fn logout() {
    config::set_value("credentials", None);
    config::set_value("webapi_token", None);
    let home = std::env::var("HOME").unwrap_or_default();
    let _ = std::fs::remove_file(format!("{home}/.cache/spotify-client/credentials.json"));
}

pub struct PendingLogin {
    pub url: String,
    verifier: PkceCodeVerifier,
}

impl PendingLogin {
    pub fn begin() -> Self {
        let (url, verifier) = oauth::generate_session_auth_url(PORT);
        Self { url, verifier }
    }

    pub fn finish(self) -> Result<(), String> {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), PORT);
        let code = oauth::get_authcode_listener(addr, Duration::from_secs(300))
            .map_err(|e| e.to_string())?;
        // `exchange_session_code_for_token` hace `expect` si Spotify lo rechaza
        let token = std::panic::catch_unwind(|| oauth::exchange_session_code_for_token(PORT, code, self.verifier))
            .map_err(|_| "Spotify rechazó el código de login".to_string())?;

        // el handshake con el AP falla a veces de forma transitoria
        let mut last_error = String::new();
        for _ in 0..3 {
            let config = SessionConfig {
                login_creds: Credentials::from_access_token(token.clone()),
                proxy_url: None,
            };
            match SessionConnection::open(config) {
                Ok(connection) => {
                    let value = serde_json::to_value(&connection.credentials).map_err(|e| e.to_string())?;
                    config::set_value("credentials", Some(value));
                    return Ok(());
                }
                Err(e) => last_error = e.to_string(),
            }
        }
        Err(format!("no se pudo iniciar sesión: {last_error}"))
    }
}
