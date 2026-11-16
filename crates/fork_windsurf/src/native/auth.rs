//! Exchanges the long-lived `api_key` for the short-lived `user_jwt` that every
//! `GetTab` must carry. The token is kept in memory only and fetched again 60
//! seconds before it expires.

use std::time::{Duration, SystemTime};

use base64::Engine as _;
use futures::lock::Mutex;
use serde::Deserialize;

use super::{ClientConfig, GET_USER_JWT_PATH, connect::ConnectClient, connect::ConnectError};
use crate::proto::exa::auth_pb::{GetUserJwtRequest, GetUserJwtResponse};

const REFRESH_MARGIN: Duration = Duration::from_secs(60);
/// The observed lifetime of a token, used when its `exp` claim can't be read.
const FALLBACK_LIFETIME: Duration = Duration::from_secs(15 * 60);

struct CachedToken {
    api_key: String,
    user_jwt: String,
    expires_at: SystemTime,
}

#[derive(Default)]
pub struct Auth {
    cached: Mutex<Option<CachedToken>>,
}

impl Auth {
    /// Returns a token for `api_key`, fetching a new one when there is none,
    /// it belongs to a different key, or it expires within a minute. Concurrent
    /// callers wait for the same fetch.
    pub async fn user_jwt(
        &self,
        connect: &ConnectClient,
        config: &ClientConfig,
        api_key: &str,
    ) -> Result<String, ConnectError> {
        let mut cached = self.cached.lock().await;
        let now = SystemTime::now();
        if let Some(token) = cached.as_ref()
            && token.api_key == api_key
            && now + REFRESH_MARGIN < token.expires_at
        {
            return Ok(token.user_jwt.clone());
        }
        *cached = None;

        let request = GetUserJwtRequest {
            metadata: Some(super::metadata(config, api_key, None, None)),
        };
        let url = format!("{}{}", config.api_server_url, GET_USER_JWT_PATH);
        let response: GetUserJwtResponse =
            connect.unary(&url, &request, config.unary_timeout).await?;
        if response.user_jwt.is_empty() {
            return Err(ConnectError::Protocol(
                "GetUserJwt returned an empty token".to_string(),
            ));
        }
        let expires_at = token_expiry(&response.user_jwt).unwrap_or(now + FALLBACK_LIFETIME);
        *cached = Some(CachedToken {
            api_key: api_key.to_string(),
            user_jwt: response.user_jwt.clone(),
            expires_at,
        });
        Ok(response.user_jwt)
    }

    pub async fn clear(&self) {
        *self.cached.lock().await = None;
    }
}

#[derive(Deserialize)]
struct Claims {
    exp: Option<u64>,
}

/// Reads the `exp` claim of a JWT without verifying it.
pub fn token_expiry(token: &str) -> Option<SystemTime> {
    let payload = token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let claims: Claims = serde_json::from_slice(&decoded).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(claims.exp?))
}

#[cfg(test)]
pub(crate) fn fake_token(expires_at: SystemTime) -> String {
    let seconds = expires_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.signature",
        engine.encode(br#"{"alg":"HS256","typ":"JWT"}"#),
        engine.encode(format!(r#"{{"exp":{seconds}}}"#))
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use futures::{FutureExt as _, executor::block_on};
    use http_client::{AsyncBody, FakeHttpClient, Response};

    use super::*;

    fn client_returning(tokens: Vec<String>, calls: Arc<AtomicUsize>) -> ConnectClient {
        let http = FakeHttpClient::create(move |request| {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            let token = tokens[index.min(tokens.len() - 1)].clone();
            async move {
                assert!(request.uri().path().ends_with("/GetUserJwt"));
                let response = GetUserJwtResponse {
                    user_jwt: token,
                    ..Default::default()
                };
                Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from(prost::Message::encode_to_vec(&response)))?)
            }
        });
        ConnectClient::new(http, Arc::new(|_| futures::future::pending().boxed()))
    }

    #[test]
    fn test_token_expiry_reads_exp_claim() {
        let expires_at = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        assert_eq!(token_expiry(&fake_token(expires_at)), Some(expires_at));
        assert_eq!(token_expiry("not-a-jwt"), None);
        assert_eq!(token_expiry("a.%%%.c"), None);
    }

    #[test]
    fn test_token_is_reused_until_close_to_expiry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let long_lived = fake_token(SystemTime::now() + Duration::from_secs(900));
        let connect = client_returning(vec![long_lived.clone()], calls.clone());
        let auth = Auth::default();
        let config = ClientConfig::default();
        for _ in 0..3 {
            assert_eq!(
                block_on(auth.user_jwt(&connect, &config, "key")),
                Ok(long_lived.clone())
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // A different key gets its own token.
        block_on(auth.user_jwt(&connect, &config, "other key")).expect("token");
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        block_on(auth.clear());
        block_on(auth.user_jwt(&connect, &config, "other key")).expect("token");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_token_expiring_within_a_minute_is_refreshed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let expiring = fake_token(SystemTime::now() + Duration::from_secs(30));
        let fresh = fake_token(SystemTime::now() + Duration::from_secs(900));
        let connect = client_returning(vec![expiring.clone(), fresh.clone()], calls.clone());
        let auth = Auth::default();
        let config = ClientConfig::default();
        assert_eq!(
            block_on(auth.user_jwt(&connect, &config, "key")),
            Ok(expiring)
        );
        assert_eq!(block_on(auth.user_jwt(&connect, &config, "key")), Ok(fresh));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_empty_token_is_a_protocol_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let connect = client_returning(vec![String::new()], calls);
        let result = block_on(Auth::default().user_jwt(&connect, &ClientConfig::default(), "key"));
        assert!(matches!(result, Err(ConnectError::Protocol(_))));
    }
}
