use hmac::{Hmac, Mac};
use iii_sdk::errors::Error;
use iii_sdk::{IIIClient, RegisterFunction, protocol::TriggerRequest, register_worker};
use rand::RngExt;
use serde_json::{Value, json};
use sha2::Sha256;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

const NONCE_TTL_MS: u64 = 5 * 60 * 1000;
const CHALLENGE_WINDOW_MS: u64 = 60 * 1000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn body_or_self(input: &Value) -> Value {
    input.get("body").cloned().unwrap_or_else(|| input.clone())
}

fn authorize_token(expected: &str, token: &str) -> bool {
    if expected.is_empty() || token.is_empty() {
        return false;
    }
    bool::from(token.as_bytes().ct_eq(expected.as_bytes()))
}

fn service_credential() -> Result<String, Error> {
    std::env::var("AGENTOS_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            Error::Handler(
                "AGENTOS_API_KEY is not configured; the MAP worker cannot authenticate its \
                 callers and cannot read shared secrets from the vault. This is a fail-closed \
                 refusal, not a bug: set AGENTOS_API_KEY for the whole stack."
                    .into(),
            )
        })
}

fn require_auth(input: &Value) -> Result<(), Error> {
    let expected = service_credential()?;
    let header = input
        .get("headers")
        .and_then(|h| h.get("authorization"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let token = header.strip_prefix("Bearer ").unwrap_or(header);
    if authorize_token(&expected, token) {
        Ok(())
    } else {
        Err(Error::Handler("Unauthorized".into()))
    }
}

/// Read `map:<agent>`'s shared secret out of the vault.
///
/// `vault::get` requires the AgentOS bearer on every call (a bus caller used to
/// be able to read plaintext secrets because the check was skipped whenever the
/// payload had no `headers` key). This worker therefore presents its OWN
/// service credential, and only after it has already authenticated its caller
/// with the same credential — it never forwards a caller-supplied header.
/// Errors name the failing step so a missing secret cannot be mistaken for an
/// authentication problem or vice versa.
async fn shared_secret(iii: &IIIClient, agent: &str) -> Result<String, Error> {
    let credential = service_credential()?;
    let entry = iii
        .trigger(TriggerRequest {
            function_id: "vault::get".to_string(),
            payload: json!({
                "key": format!("map:{agent}"),
                "headers": { "authorization": format!("Bearer {credential}") },
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| {
            Error::Handler(format!(
                "vault::get for map:{agent} failed: {e}. The vault must be unlocked \
                 (vault::init) and hold a `map:{agent}` entry."
            ))
        })?;

    match entry.get("value").and_then(|v| v.as_str()) {
        Some(secret) if !secret.is_empty() => Ok(secret.to_string()),
        _ => Err(Error::Handler(format!(
            "no shared secret stored for map:{agent}"
        ))),
    }
}

fn random_nonce_hex() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    hex::encode(bytes)
}

fn hmac_hex(secret: &str, payload: &str) -> Result<String, Error> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|e| Error::Handler(format!("HMAC key error: {e}")))?;
    mac.update(payload.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

async fn map_challenge(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    require_auth(&input)?;
    let body = body_or_self(&input);
    let source_agent = body
        .get("sourceAgent")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let target_agent = body
        .get("targetAgent")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if source_agent.is_empty() || target_agent.is_empty() {
        return Err(Error::Handler(
            "sourceAgent and targetAgent are required".into(),
        ));
    }

    let nonce = random_nonce_hex();
    let timestamp = now_ms();

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": "map_challenges",
            "key": &nonce,
            "value": {
                "nonce": &nonce,
                "timestamp": timestamp,
                "sourceAgent": source_agent,
                "targetAgent": target_agent,
            },
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    let nonce_short = nonce.chars().take(8).collect::<String>();
    let source = source_agent.to_string();
    let target = target_agent.to_string();
    let _iii = iii.clone();
    tokio::spawn(async move {
        let _ = _iii
            .trigger(TriggerRequest {
                function_id: "security::audit".to_string(),
                payload: json!({
                    "type": "map_challenge_issued",
                    "detail": {
                        "sourceAgent": source,
                        "targetAgent": target,
                        "nonce": nonce_short,
                    },
                }),
                action: None,
                timeout_ms: None,
            })
            .await;
    });

    Ok(json!({
        "nonce": nonce,
        "timestamp": timestamp,
        "sourceAgent": source_agent,
    }))
}

/// Sign a MAP challenge with the responder's shared secret.
///
/// DO NOT REMOVE THE AUTH CHECK BELOW. This function is a signing oracle by
/// construction: it will produce a valid `security::map_verify` response for
/// ANY `responderAgent` whose secret this host's vault holds. Unauthenticated,
/// it let any local process on the engine bus forge mutual-auth responses for
/// every identity on the machine (2026-09-02 review, H-2). If it ever appears
/// "broken", the cause is a missing AGENTOS_API_KEY or a locked vault — both of
/// which the errors below name explicitly — never the auth check.
async fn map_respond(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    require_auth(&input)?;
    let body = body_or_self(&input);
    let nonce = body.get("nonce").and_then(|v| v.as_str()).unwrap_or("");
    let source_agent = body
        .get("sourceAgent")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let responder_agent = body
        .get("responderAgent")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let timestamp = body.get("timestamp").and_then(|v| v.as_u64()).unwrap_or(0);

    if nonce.is_empty() || source_agent.is_empty() || responder_agent.is_empty() {
        return Err(Error::Handler(
            "nonce, sourceAgent, and responderAgent are required".into(),
        ));
    }

    let secret = shared_secret(iii, responder_agent).await?;

    let payload = format!("{nonce}:{source_agent}:{responder_agent}:{timestamp}");
    let signature = hmac_hex(&secret, &payload)?;

    // Every signature this worker issues leaves a trace. An oracle that signs
    // silently is indistinguishable from one that is never used.
    audit_emit(
        iii,
        "map_response_signed",
        json!({
            "sourceAgent": source_agent,
            "responderAgent": responder_agent,
            "nonce": nonce.chars().take(8).collect::<String>(),
        }),
    )
    .await;

    Ok(json!({
        "signature": signature,
        "nonce": nonce,
        "responderAgent": responder_agent,
    }))
}

async fn audit_emit(iii: &IIIClient, entry_type: &str, detail: Value) {
    let _iii = iii.clone();
    let entry_type = entry_type.to_string();
    tokio::spawn(async move {
        let _ = _iii
            .trigger(TriggerRequest {
                function_id: "security::audit".to_string(),
                payload: json!({ "type": entry_type, "detail": detail }),
                action: None,
                timeout_ms: None,
            })
            .await;
    });
}

async fn map_verify(iii: &IIIClient, input: Value) -> Result<Value, Error> {
    // Unconditional: a bus caller never carries a `headers` object, so the old
    // `if input.get("headers").is_some()` guard meant this ran unauthenticated
    // for every caller that mattered.
    require_auth(&input)?;
    let body = body_or_self(&input);
    let nonce = body.get("nonce").and_then(|v| v.as_str()).unwrap_or("");
    let signature = body.get("signature").and_then(|v| v.as_str()).unwrap_or("");
    let responder_agent = body
        .get("responderAgent")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if nonce.is_empty() || signature.is_empty() || responder_agent.is_empty() {
        return Err(Error::Handler(
            "nonce, signature, and responderAgent are required".into(),
        ));
    }

    let challenge: Value = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({ "scope": "map_challenges", "key": nonce }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!(null));

    if challenge.is_null() || challenge.as_object().map(|o| o.is_empty()).unwrap_or(false) {
        let _ = iii
            .trigger(TriggerRequest {
                function_id: "state::delete".to_string(),
                payload: json!({ "scope": "map_challenges", "key": nonce }),
                action: None,
                timeout_ms: None,
            })
            .await;
        audit_emit(
            iii,
            "map_verify_failed",
            json!({ "reason": "unknown_nonce", "responderAgent": responder_agent }),
        )
        .await;
        return Ok(json!({ "verified": false, "reason": "unknown_nonce" }));
    }

    let challenge_ts = challenge
        .get("timestamp")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let source_agent = challenge
        .get("sourceAgent")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if now_ms().saturating_sub(challenge_ts) > CHALLENGE_WINDOW_MS {
        let _ = iii
            .trigger(TriggerRequest {
                function_id: "state::delete".to_string(),
                payload: json!({ "scope": "map_challenges", "key": nonce }),
                action: None,
                timeout_ms: None,
            })
            .await;
        audit_emit(
            iii,
            "map_verify_failed",
            json!({ "reason": "expired", "responderAgent": responder_agent }),
        )
        .await;
        return Ok(json!({ "verified": false, "reason": "challenge_expired" }));
    }

    let used: Value = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({ "scope": "map_used_nonces", "key": nonce }),
            action: None,
            timeout_ms: None,
        })
        .await
        .unwrap_or(json!(null));

    if !(used.is_null() || used.as_object().map(|o| o.is_empty()).unwrap_or(false)) {
        let _ = iii
            .trigger(TriggerRequest {
                function_id: "state::delete".to_string(),
                payload: json!({ "scope": "map_challenges", "key": nonce }),
                action: None,
                timeout_ms: None,
            })
            .await;
        audit_emit(
            iii,
            "map_verify_failed",
            json!({ "reason": "replay_detected", "responderAgent": responder_agent }),
        )
        .await;
        return Ok(json!({ "verified": false, "reason": "replay_detected" }));
    }

    let secret = match shared_secret(iii, responder_agent).await {
        Ok(secret) => secret,
        Err(error) => {
            // A verification with no usable secret is a failed verification,
            // not a handler error — but say which, so an operator can tell a
            // missing entry from a locked vault.
            tracing::warn!(responder_agent, error = %error, "map_verify: no usable shared secret");
            return Ok(json!({
                "verified": false,
                "reason": "no_shared_secret",
                "detail": error.to_string(),
            }));
        }
    };

    let payload = format!("{nonce}:{source_agent}:{responder_agent}:{challenge_ts}");
    let expected = hmac_hex(&secret, &payload)?;

    let expected_bytes = hex::decode(&expected).unwrap_or_default();
    let signature_bytes = hex::decode(signature).unwrap_or_default();
    let verified = expected_bytes.len() == signature_bytes.len()
        && bool::from(expected_bytes.ct_eq(&signature_bytes));

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": "map_used_nonces",
            "key": nonce,
            "value": { "usedAt": now_ms(), "responderAgent": responder_agent },
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    if let Err(e) = iii
        .trigger(TriggerRequest {
            function_id: "state::delete".to_string(),
            payload: json!({ "scope": "map_challenges", "key": nonce }),
            action: None,
            timeout_ms: None,
        })
        .await
    {
        tracing::warn!(
            error = %e,
            responder_agent = %responder_agent,
            "map_verify: failed to delete consumed challenge; nonce already recorded as used"
        );
    }

    let nonce_owned = nonce.to_string();
    let _iii = iii.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(NONCE_TTL_MS)).await;
        let _ = _iii
            .trigger(TriggerRequest {
                function_id: "state::delete".to_string(),
                payload: json!({ "scope": "map_used_nonces", "key": nonce_owned }),
                action: None,
                timeout_ms: None,
            })
            .await;
    });

    audit_emit(
        iii,
        if verified {
            "map_verify_success"
        } else {
            "map_verify_failed"
        },
        json!({
            "responderAgent": responder_agent,
            "sourceAgent": source_agent,
        }),
    )
    .await;

    if verified {
        Ok(json!({ "verified": true, "agent": responder_agent }))
    } else {
        Ok(json!({ "verified": false }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());

    let iii_ref = iii.clone();
    iii.register_function(
        "security::map_challenge",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { map_challenge(&iii, input).await }
        })
        .description("Generate MAP mutual-auth challenge nonce"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "security::map_respond",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { map_respond(&iii, input).await }
        })
        .description("Sign MAP challenge nonce with shared secret"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "security::map_verify",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move { map_verify(&iii, input).await }
        })
        .description("Verify MAP mutual-auth response signature"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "security::map_challenge".to_string(),
        json!({ "api_path": "api/security/map/challenge", "http_method": "POST" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "security::map_verify".to_string(),
        json!({ "api_path": "api/security/map/verify", "http_method": "POST" }),
        None,
    )?;

    tracing::info!("security-map worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(future)
    }

    fn with_api_key<T>(value: Option<&str>, test: impl FnOnce() -> T) -> T {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = std::env::var_os("AGENTOS_API_KEY");
        unsafe {
            match value {
                Some(value) => std::env::set_var("AGENTOS_API_KEY", value),
                None => std::env::remove_var("AGENTOS_API_KEY"),
            }
        }
        let result = test();
        unsafe {
            match previous {
                Some(value) => std::env::set_var("AGENTOS_API_KEY", value),
                None => std::env::remove_var("AGENTOS_API_KEY"),
            }
        }
        result
    }

    /// Never connected: any handler that reaches the bus would block on the SDK
    /// timeout, so every assertion here must return before the first call.
    fn offline_client() -> IIIClient {
        IIIClient::new("ws://127.0.0.1:1")
    }

    #[test]
    fn map_respond_refuses_an_unauthenticated_bus_caller() {
        let request = json!({
            "nonce": "aa",
            "sourceAgent": "attacker",
            "responderAgent": "victim",
            "timestamp": 1u64,
        });
        let error = with_api_key(Some("map-expected"), || {
            block_on(map_respond(&offline_client(), request))
        })
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("Unauthorized"),
            "map_respond must not sign for a caller with no bearer, got: {error}"
        );
    }

    #[test]
    fn map_respond_refuses_a_wrong_bearer() {
        let request = json!({
            "headers": { "authorization": "Bearer wrong" },
            "nonce": "aa",
            "sourceAgent": "attacker",
            "responderAgent": "victim",
            "timestamp": 1u64,
        });
        let error = with_api_key(Some("map-expected-2"), || {
            block_on(map_respond(&offline_client(), request))
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("Unauthorized"), "got: {error}");
    }

    #[test]
    fn map_challenge_and_verify_refuse_an_unauthenticated_bus_caller() {
        with_api_key(Some("map-expected-3"), || {
            let challenge = block_on(map_challenge(
                &offline_client(),
                json!({ "sourceAgent": "a", "targetAgent": "b" }),
            ))
            .unwrap_err()
            .to_string();
            assert!(challenge.contains("Unauthorized"), "{challenge}");

            let verify = block_on(map_verify(
                &offline_client(),
                json!({ "nonce": "aa", "signature": "bb", "responderAgent": "b" }),
            ))
            .unwrap_err()
            .to_string();
            assert!(verify.contains("Unauthorized"), "{verify}");
        });
    }

    #[test]
    fn a_missing_service_credential_is_a_named_refusal_not_a_silent_failure() {
        with_api_key(None, || {
            let error = service_credential().unwrap_err().to_string();
            assert!(error.contains("AGENTOS_API_KEY"), "{error}");
            assert!(
                error.contains("fail-closed"),
                "the message must tell a maintainer this is deliberate: {error}"
            );
            // and the handlers surface the same message rather than "Unauthorized"
            let respond = block_on(map_respond(
                &offline_client(),
                json!({
                    "headers": { "authorization": "Bearer anything" },
                    "nonce": "aa",
                    "sourceAgent": "a",
                    "responderAgent": "b",
                    "timestamp": 1u64,
                }),
            ))
            .unwrap_err()
            .to_string();
            assert!(respond.contains("AGENTOS_API_KEY"), "{respond}");
        });
        with_api_key(Some(""), || {
            assert!(service_credential().is_err(), "an empty key is not a key");
        });
    }

    #[test]
    fn authenticated_map_respond_still_validates_its_arguments() {
        // Proves the auth check runs FIRST and that a valid bearer reaches the
        // argument validation rather than being rejected outright.
        let error = with_api_key(Some("map-args"), || {
            block_on(map_respond(
                &offline_client(),
                json!({ "headers": { "authorization": "Bearer map-args" } }),
            ))
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("are required"), "got: {error}");
    }

    #[test]
    fn authorization_rejects_empty_credentials_and_accepts_a_match() {
        assert!(!authorize_token("", ""));
        assert!(!authorize_token("", "token"));
        assert!(!authorize_token("expected", ""));
        assert!(!authorize_token("expected", "different"));
        assert!(authorize_token("expected", "expected"));
    }

    #[test]
    fn test_random_nonce_hex_length() {
        let nonce = random_nonce_hex();
        assert_eq!(nonce.len(), 64);
    }

    #[test]
    fn test_random_nonce_unique() {
        let n1 = random_nonce_hex();
        let n2 = random_nonce_hex();
        assert_ne!(n1, n2);
    }

    #[test]
    fn test_random_nonce_is_hex() {
        let nonce = random_nonce_hex();
        assert!(nonce.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_hmac_hex_deterministic() {
        let h1 = hmac_hex("secret", "payload").unwrap();
        let h2 = hmac_hex("secret", "payload").unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_hmac_hex_different_secrets() {
        let h1 = hmac_hex("secret1", "payload").unwrap();
        let h2 = hmac_hex("secret2", "payload").unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_hmac_hex_different_payloads() {
        let h1 = hmac_hex("secret", "p1").unwrap();
        let h2 = hmac_hex("secret", "p2").unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_hmac_hex_64_chars() {
        let h = hmac_hex("k", "v").unwrap();
        assert_eq!(h.len(), 64);
    }

    #[test]
    fn test_now_ms_nonzero() {
        assert!(now_ms() > 0);
    }

    #[test]
    fn test_body_or_self_with_body() {
        let v = json!({ "body": { "x": 1 } });
        assert_eq!(body_or_self(&v), json!({ "x": 1 }));
    }

    #[test]
    fn test_body_or_self_no_body() {
        let v = json!({ "y": 2 });
        assert_eq!(body_or_self(&v), json!({ "y": 2 }));
    }

    #[test]
    fn test_authorize_token_rejects_empty_expected_and_token() {
        assert!(!authorize_token("", ""));
    }

    #[test]
    fn test_authorize_token_rejects_empty_expected() {
        assert!(!authorize_token("", "token"));
    }

    #[test]
    fn test_authorize_token_rejects_empty_token() {
        assert!(!authorize_token("expected", ""));
    }

    #[test]
    fn test_authorize_token_rejects_mismatch() {
        assert!(!authorize_token("expected", "different"));
    }

    #[test]
    fn test_authorize_token_accepts_match() {
        assert!(authorize_token("expected", "expected"));
    }

    #[test]
    fn test_constant_time_eq_match() {
        let a = b"abcdef";
        let b = b"abcdef";
        assert!(bool::from(a.ct_eq(b)));
    }

    #[test]
    fn test_constant_time_eq_no_match() {
        let a = b"abcdef";
        let b = b"abcdeg";
        assert!(!bool::from(a.ct_eq(b)));
    }

    #[test]
    fn test_signature_format_concat() {
        let nonce = "n1";
        let src = "alice";
        let resp = "bob";
        let ts = 12345u64;
        let payload = format!("{nonce}:{src}:{resp}:{ts}");
        assert_eq!(payload, "n1:alice:bob:12345");
    }

    #[test]
    fn test_constants_in_milliseconds() {
        assert_eq!(NONCE_TTL_MS, 300_000);
        assert_eq!(CHALLENGE_WINDOW_MS, 60_000);
    }
}
