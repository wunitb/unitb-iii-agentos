//! Client half: the handshake credential every in-tree worker presents.
//!
//! `iii-sdk` has no environment hook for handshake headers — `InitOptions
//! { headers }` is the only path, and it is per-call. Before this module all 62
//! workers connected with `InitOptions::default()`, i.e. anonymously, which is
//! why enabling `rbac.auth_function_id` would have locked the product out of its
//! own bus. Workers now call [`init_options`] instead.
//!
//! The credential is `AGENTOS_API_KEY`: the key the CLI already generates at
//! first run into a 0600 `.env` and already exports to every worker. No new
//! secret store, nothing in `config.yaml`, and no default value — when the
//! variable is absent no header is sent, the worker still connects, and the bus
//! policy simply files it under the untrusted tier.

use std::collections::HashMap;

use iii_sdk::InitOptions;

/// Handshake header carrying the bus credential.
pub const AUTHORIZATION_HEADER: &str = "authorization";

/// Handshake headers for this process, or `None` when no credential is set.
pub fn handshake_headers() -> Option<HashMap<String, String>> {
    let key = crate::policy::expected_api_key()?;
    Some(HashMap::from([(
        AUTHORIZATION_HEADER.to_string(),
        format!("Bearer {key}"),
    )]))
}

/// `InitOptions` for `iii_sdk::register_worker`, carrying the bus credential.
///
/// Drop-in replacement for `InitOptions::default()`:
///
/// ```no_run
/// use iii_sdk::register_worker;
///
/// let iii = register_worker("ws://127.0.0.1:49134", agentos_bus_auth::init_options());
/// ```
///
/// When `AGENTOS_API_KEY` is unset this is exactly `InitOptions::default()`, so
/// a stack without bus RBAC behaves as it did before.
pub fn init_options() -> InitOptions {
    InitOptions {
        headers: handshake_headers(),
        ..InitOptions::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{API_KEY_ENV, TIER_TRUSTED, TIER_UNTRUSTED, auth_result, tier_for};
    use serde_json::json;

    /// `std::env::set_var` is unsafe in edition 2024 and process-global, and
    /// `cargo test` runs these in parallel threads, so the guard serialises them
    /// and restores the previous value. Without the lock the "no key" case reads
    /// the key another test just set and passes for the wrong reason.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_api_key<T>(value: Option<&str>, body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os(API_KEY_ENV);
        unsafe {
            match value {
                Some(value) => std::env::set_var(API_KEY_ENV, value),
                None => std::env::remove_var(API_KEY_ENV),
            }
        }
        let result = body();
        unsafe {
            match previous {
                Some(value) => std::env::set_var(API_KEY_ENV, value),
                None => std::env::remove_var(API_KEY_ENV),
            }
        }
        result
    }

    #[test]
    fn the_credential_travels_as_a_bearer_header() {
        with_api_key(Some("k-123"), || {
            let headers = handshake_headers().expect("a key was configured");
            assert_eq!(headers.get("authorization"), Some(&"Bearer k-123".into()));
            assert_eq!(
                init_options()
                    .headers
                    .expect("init_options carries the header")
                    .len(),
                1
            );
        });
    }

    #[test]
    fn no_key_means_no_header_and_no_fabricated_credential() {
        with_api_key(None, || {
            assert!(handshake_headers().is_none());
            assert!(init_options().headers.is_none());
        });
        with_api_key(Some(""), || {
            assert!(
                handshake_headers().is_none(),
                "an empty key must not become `Bearer `"
            );
        });
    }

    /// The two halves have to agree: what the client sends must be what the
    /// daemon's policy calls trusted. This is the round-trip.
    #[test]
    fn what_the_client_sends_is_what_the_daemon_trusts() {
        with_api_key(Some("round-trip-key"), || {
            let sent = handshake_headers().expect("a key was configured");
            let auth_input = json!({
                "headers": sent,
                "query_params": {},
                "ip_address": "127.0.0.1",
            });
            assert_eq!(tier_for(&auth_input, Some("round-trip-key")), TIER_TRUSTED);
            assert_eq!(
                auth_result(&auth_input, Some("round-trip-key"))["forbidden_functions"],
                json!([])
            );
            assert_eq!(
                tier_for(&auth_input, Some("a-different-key")),
                TIER_UNTRUSTED
            );
        });
    }
}
