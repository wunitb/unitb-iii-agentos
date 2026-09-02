//! The bus-auth daemon: the engine side of the iii-sdk protocol.
//!
//! The engine's builtin `iii-bridge` worker connects here as a client and
//! forwards three function ids (see [`crate::policy`]). Everything else on this
//! socket is answered with `function_not_found`, so the daemon is a policy
//! oracle and nothing more: it registers nothing, it stores nothing, and it
//! cannot be used to reach the bus.
//!
//! # Threat notes
//!
//! * Bind loopback only. Any local process can open this socket and ask
//!   `agentos::bus_auth` whether a token is valid; that is a 256-bit-key oracle,
//!   not a bypass, but it is a reason to keep the socket off every other
//!   interface.
//! * A `RegisterFunction` frame from the peer is ignored. The daemon is not an
//!   engine and must never behave like one.
//! * If the daemon is down, the engine's forward call errors and the RBAC gate
//!   refuses every new bus connection. That is the intended direction (fail
//!   closed) and it is why `agentos up` has to start this before the engine.

use std::net::SocketAddr;

use anyhow::Context as _;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::policy::{
    AUTH_FUNCTION_ID, FUNCTION_REGISTRATION_HOOK_ID, TRIGGER_REGISTRATION_HOOK_ID, auth_result,
    function_registration_allowed, tier_of_context, trigger_registration_allowed,
};

/// Default loopback address the daemon listens on.
pub const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:49129";

/// Function id the SDK client sends once per connection to announce itself.
const WORKER_REGISTER_FUNCTION_ID: &str = "engine::workers::register";

/// Build the reply to one protocol frame, or `None` when the frame needs no
/// answer.
///
/// Pure on purpose: the whole policy surface is testable without a socket.
pub fn handle_frame(text: &str, expected_key: Option<&str>) -> Option<String> {
    let frame: Value = serde_json::from_str(text).ok()?;
    match frame.get("type").and_then(Value::as_str)? {
        "ping" => Some(json!({ "type": "pong" }).to_string()),
        "invokefunction" => {
            // A `Void` trigger carries no invocation_id and expects no answer.
            let invocation_id = frame.get("invocation_id")?.as_str()?.to_string();
            let function_id = frame.get("function_id").and_then(Value::as_str)?;
            let data = frame.get("data").cloned().unwrap_or(Value::Null);
            Some(match invoke(function_id, &data, expected_key) {
                Ok(result) => json!({
                    "type": "invocationresult",
                    "invocation_id": invocation_id,
                    "function_id": function_id,
                    "result": result,
                })
                .to_string(),
                Err(error) => json!({
                    "type": "invocationresult",
                    "invocation_id": invocation_id,
                    "function_id": function_id,
                    "error": error,
                })
                .to_string(),
            })
        }
        _ => None,
    }
}

/// Dispatch one forwarded function call.
fn invoke(function_id: &str, data: &Value, expected_key: Option<&str>) -> Result<Value, Value> {
    match function_id {
        AUTH_FUNCTION_ID => {
            let result = auth_result(data, expected_key);
            tracing::info!(
                tier = %result["context"][crate::policy::TIER_CONTEXT_KEY],
                ip = %data.get("ip_address").and_then(serde_json::Value::as_str).unwrap_or("?"),
                "bus connection authenticated"
            );
            Ok(result)
        }
        FUNCTION_REGISTRATION_HOOK_ID => {
            let target = data
                .get("function_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let context = data.get("context").cloned().unwrap_or(Value::Null);
            if function_registration_allowed(target, &context) {
                // Debug, not info: on a full boot this is ~700 lines. It is also
                // how `tests/registry_surface.txt` is captured — see that file.
                tracing::debug!(
                    function_id = %target,
                    tier = %tier_of_context(&context),
                    "allowed function registration"
                );
                Ok(json!({ "function_id": target }))
            } else {
                tracing::warn!(
                    function_id = %target,
                    tier = %tier_of_context(&context),
                    "refused function registration"
                );
                Err(denied("function_registration_denied", target))
            }
        }
        TRIGGER_REGISTRATION_HOOK_ID => {
            let target = data
                .get("function_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let context = data.get("context").cloned().unwrap_or(Value::Null);
            if trigger_registration_allowed(target, &context) {
                tracing::debug!(
                    function_id = %target,
                    trigger_id = %data.get("trigger_id").and_then(serde_json::Value::as_str).unwrap_or("?"),
                    tier = %tier_of_context(&context),
                    "allowed trigger registration"
                );
                Ok(json!({ "function_id": target }))
            } else {
                tracing::warn!(
                    function_id = %target,
                    trigger_id = %data.get("trigger_id").and_then(serde_json::Value::as_str).unwrap_or("?"),
                    tier = %tier_of_context(&context),
                    "refused trigger registration"
                );
                Err(denied("trigger_registration_denied", target))
            }
        }
        // The bridge announces itself on connect; anything else is not ours.
        WORKER_REGISTER_FUNCTION_ID => Ok(json!({ "success": true })),
        other => Err(json!({
            "code": "function_not_found",
            "message": format!("the bus-auth daemon does not serve '{other}'"),
        })),
    }
}

fn denied(code: &str, function_id: &str) -> Value {
    json!({
        "code": code,
        "message": format!(
            "'{function_id}' is not registrable by a session without the bus credential"
        ),
    })
}

/// Serve one accepted TCP connection for its lifetime.
pub async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    expected_key: Option<String>,
) -> anyhow::Result<()> {
    let mut socket = tokio_tungstenite::accept_async(stream)
        .await
        .with_context(|| format!("websocket handshake with {peer}"))?;
    tracing::debug!(%peer, "bridge connected");

    // The SDK client waits for its worker id before it sends anything.
    let hello = json!({
        "type": "workerregistered",
        "worker_id": uuid::Uuid::new_v4().to_string(),
        "reattach_token": uuid::Uuid::new_v4().to_string(),
    });
    socket
        .send(WsMessage::Text(hello.to_string().into()))
        .await?;

    while let Some(frame) = socket.next().await {
        match frame? {
            WsMessage::Text(text) => {
                if let Some(reply) = handle_frame(&text, expected_key.as_deref()) {
                    socket.send(WsMessage::Text(reply.into())).await?;
                }
            }
            WsMessage::Ping(payload) => socket.send(WsMessage::Pong(payload)).await?,
            WsMessage::Close(_) => break,
            // Binary frames on this socket are OTEL noise from the SDK.
            _ => {}
        }
    }
    tracing::debug!(%peer, "bridge disconnected");
    Ok(())
}

/// Accept loop. Never returns while the listener is alive.
pub async fn serve(listener: TcpListener, expected_key: String) -> anyhow::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let expected_key = Some(expected_key.clone());
        tokio::spawn(async move {
            if let Err(error) = serve_connection(stream, peer, expected_key).await {
                tracing::warn!(%peer, %error, "bus-auth connection ended with an error");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{TIER_CONTEXT_KEY, TIER_TRUSTED, TIER_UNTRUSTED};

    fn invoke_frame(function_id: &str, data: Value) -> String {
        json!({
            "type": "invokefunction",
            "invocation_id": "0f9d5a1e-0000-4000-8000-000000000001",
            "function_id": function_id,
            "data": data,
        })
        .to_string()
    }

    fn reply(text: &str) -> Value {
        serde_json::from_str(&handle_frame(text, Some("secret")).expect("a reply")).unwrap()
    }

    #[test]
    fn auth_answers_the_engine_with_a_tier() {
        let trusted = reply(&invoke_frame(
            AUTH_FUNCTION_ID,
            json!({ "headers": { "authorization": "Bearer secret" }, "ip_address": "127.0.0.1" }),
        ));
        assert_eq!(
            trusted["result"]["context"][TIER_CONTEXT_KEY],
            json!(TIER_TRUSTED)
        );
        assert_eq!(trusted["result"]["forbidden_functions"], json!([]));
        assert_eq!(
            trusted["invocation_id"],
            json!("0f9d5a1e-0000-4000-8000-000000000001"),
            "the engine matches the answer by invocation id"
        );

        let untrusted = reply(&invoke_frame(
            AUTH_FUNCTION_ID,
            json!({ "headers": {}, "ip_address": "127.0.0.1" }),
        ));
        assert_eq!(
            untrusted["result"]["context"][TIER_CONTEXT_KEY],
            json!(TIER_UNTRUSTED)
        );
        assert!(
            untrusted["result"]["forbidden_functions"]
                .as_array()
                .unwrap()
                .contains(&json!("vault::get"))
        );
    }

    #[test]
    fn the_registration_hook_refuses_a_hijack_with_an_error() {
        let denied = reply(&invoke_frame(
            FUNCTION_REGISTRATION_HOOK_ID,
            json!({
                "function_id": "vault::get",
                "context": { TIER_CONTEXT_KEY: TIER_UNTRUSTED },
            }),
        ));
        assert_eq!(
            denied["error"]["code"],
            json!("function_registration_denied")
        );
        assert!(
            denied.get("result").is_none(),
            "an object result is what the engine reads as ALLOW"
        );

        let allowed = reply(&invoke_frame(
            FUNCTION_REGISTRATION_HOOK_ID,
            json!({
                "function_id": "vault::get",
                "context": { TIER_CONTEXT_KEY: TIER_TRUSTED },
            }),
        ));
        assert_eq!(allowed["result"], json!({ "function_id": "vault::get" }));
    }

    #[test]
    fn the_trigger_hook_refuses_a_minted_trigger_onto_a_privileged_id() {
        let denied = reply(&invoke_frame(
            TRIGGER_REGISTRATION_HOOK_ID,
            json!({
                "trigger_id": "attacker",
                "trigger_type": "cron",
                "function_id": "vault::get",
                "context": { TIER_CONTEXT_KEY: TIER_UNTRUSTED },
            }),
        ));
        assert_eq!(
            denied["error"]["code"],
            json!("trigger_registration_denied")
        );

        let allowed = reply(&invoke_frame(
            TRIGGER_REGISTRATION_HOOK_ID,
            json!({
                "trigger_id": "state-ui",
                "trigger_type": "console:script",
                "function_id": "state::ui-content",
                "context": { TIER_CONTEXT_KEY: TIER_UNTRUSTED },
            }),
        ));
        assert_eq!(allowed["result"]["function_id"], json!("state::ui-content"));
    }

    #[test]
    fn the_daemon_serves_nothing_else() {
        let other = reply(&invoke_frame("vault::get", json!({ "key": "x" })));
        assert_eq!(other["error"]["code"], json!("function_not_found"));
        let announce = reply(&invoke_frame(WORKER_REGISTER_FUNCTION_ID, json!({})));
        assert_eq!(announce["result"], json!({ "success": true }));
    }

    #[test]
    fn protocol_noise_is_ignored_and_pings_are_answered() {
        assert_eq!(
            handle_frame(r#"{"type":"ping"}"#, Some("secret")).as_deref(),
            Some(r#"{"type":"pong"}"#)
        );
        for noise in [
            "",
            "not json",
            r#"{"no":"type"}"#,
            r#"{"type":"registerfunction","id":"vault::get"}"#,
            r#"{"type":"invokefunction","function_id":"agentos::bus_auth","data":{}}"#,
        ] {
            assert!(
                handle_frame(noise, Some("secret")).is_none(),
                "{noise} must not produce a reply"
            );
        }
    }

    #[tokio::test]
    async fn a_client_gets_its_worker_id_and_a_policy_answer_over_a_real_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.expect("accept");
            serve_connection(stream, peer, Some("secret".to_string()))
                .await
                .expect("serve");
        });

        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect("connect");
        let hello: Value = match socket.next().await.expect("hello").expect("frame") {
            WsMessage::Text(text) => serde_json::from_str(&text).expect("json"),
            other => panic!("expected text, got {other:?}"),
        };
        assert_eq!(hello["type"], json!("workerregistered"));
        assert!(hello["worker_id"].as_str().is_some());

        socket
            .send(WsMessage::Text(
                invoke_frame(
                    AUTH_FUNCTION_ID,
                    json!({ "headers": { "authorization": "Bearer secret" } }),
                )
                .into(),
            ))
            .await
            .expect("send");
        let answer: Value = match socket.next().await.expect("answer").expect("frame") {
            WsMessage::Text(text) => serde_json::from_str(&text).expect("json"),
            other => panic!("expected text, got {other:?}"),
        };
        assert_eq!(
            answer["result"]["context"][TIER_CONTEXT_KEY],
            json!(TIER_TRUSTED)
        );
    }
}
