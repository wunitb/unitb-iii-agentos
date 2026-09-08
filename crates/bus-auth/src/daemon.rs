//! Pure policy handlers with an OCI worker transport for iii 0.23.
//!
//! Worker mode connects to the container-private raw manager after engine boot,
//! registers exactly the four existing policy handlers, then answers invocations.
//! The legacy listener remains available for older bridge configurations only.
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
//!   closed). OCI startup waits for policy registration before Compose/product workers.

use std::net::SocketAddr;

use anyhow::Context as _;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::policy::{
    AUTH_FUNCTION_ID, FUNCTION_REGISTRATION_HOOK_ID, TRIGGER_REGISTRATION_HOOK_ID,
    TRIGGER_TYPE_REGISTRATION_HOOK_ID, auth_result, function_registration_allowed, tier_of_context,
    trigger_registration_allowed, trigger_type_registration_allowed,
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
        TRIGGER_TYPE_REGISTRATION_HOOK_ID => {
            let target = data
                .get("trigger_type_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let context = data.get("context").cloned().unwrap_or(Value::Null);
            if trigger_type_registration_allowed(target, &context) {
                tracing::debug!(
                    trigger_type_id = %target,
                    tier = %tier_of_context(&context),
                    "allowed trigger type registration"
                );
                Ok(json!({ "trigger_type_id": target }))
            } else {
                // Loud, not debug: claiming a trigger type hands the claimant
                // every existing binding of that type and silently strands the
                // ones registered after it.
                tracing::warn!(
                    trigger_type_id = %target,
                    tier = %tier_of_context(&context),
                    "refused trigger type registration"
                );
                Err(denied("trigger_type_registration_denied", target))
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

/// Attach a policy-only worker to the fixed private manager. No proxy or SDK
/// bootstrap dependency: the wire spelling is iii 0.23's `registerfunction`.
/// Disconnects/errors terminate the worker; callers must treat it as unhealthy.
pub async fn serve_worker(url: &str, expected_key: String) -> anyhow::Result<()> {
    crate::config::require_container()?;
    anyhow::ensure!(
        url == crate::config::RAW_WORKER_URL,
        "policy worker must use the fixed private raw manager"
    );
    run_worker(url, expected_key).await
}

async fn run_worker(url: &str, expected_key: String) -> anyhow::Result<()> {
    let (mut socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .context("connect policy worker to private raw manager")?;
    let hello = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match socket.next().await.transpose()? {
                Some(WsMessage::Text(text)) => {
                    let frame: Value = serde_json::from_str(&text)?;
                    anyhow::ensure!(
                        frame["type"] != "error",
                        "engine refused policy worker: {frame}"
                    );
                    if frame["type"] == "workerregistered" {
                        return Ok::<(), anyhow::Error>(());
                    }
                }
                Some(WsMessage::Ping(payload)) => socket.send(WsMessage::Pong(payload)).await?,
                None | Some(WsMessage::Close(_)) => anyhow::bail!("engine closed policy handshake"),
                _ => {}
            }
        }
    })
    .await
    .context("policy handshake timed out")?;
    hello?;
    // Metadata registration precedes handler registration on the same socket.
    socket
        .send(WsMessage::Text(
            json!({
                "type": "invokefunction", "invocation_id": uuid::Uuid::new_v4().to_string(),
                "function_id": WORKER_REGISTER_FUNCTION_ID,
                "data": { "name": "agentos-bus-auth", "namespace": "default" }
            })
            .to_string()
            .into(),
        ))
        .await?;
    for (_, id) in crate::policy::ARMED_HOOKS {
        socket
            .send(WsMessage::Text(
                json!({ "type": "registerfunction", "id": id })
                    .to_string()
                    .into(),
            ))
            .await?;
    }
    tracing::info!(%url, "policy worker attached; four hooks submitted");
    while let Some(frame) = socket.next().await {
        match frame? {
            WsMessage::Text(text) => {
                let frame: Value = serde_json::from_str(&text)?;
                anyhow::ensure!(
                    frame["type"] != "error" && frame.get("error").is_none_or(Value::is_null),
                    "policy worker registration/connection error: {frame}"
                );
                if let Some(reply) = handle_frame(&text, Some(&expected_key)) {
                    socket.send(WsMessage::Text(reply.into())).await?;
                }
            }
            WsMessage::Ping(payload) => socket.send(WsMessage::Pong(payload)).await?,
            WsMessage::Close(_) => break,
            _ => {}
        }
    }
    anyhow::bail!("private raw manager disconnected the policy worker")
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn policy_worker_registers_only_four_handlers_and_fails_on_disconnect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let worker = tokio::spawn(async move { super::run_worker(&url, "secret".into()).await });
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(WsMessage::Text(
                json!({"type": "workerregistered", "worker_id": "fixture"})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        let metadata: Value =
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(metadata["function_id"], WORKER_REGISTER_FUNCTION_ID);
        assert_eq!(metadata["data"]["namespace"], "default");
        for (_, id) in crate::policy::ARMED_HOOKS {
            let registration: Value =
                serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(registration, json!({"type": "registerfunction", "id": id}));
        }
        socket
            .send(WsMessage::Text(
                invoke_frame(AUTH_FUNCTION_ID, json!({"headers": {}})).into(),
            ))
            .await
            .unwrap();
        let result: Value =
            serde_json::from_str(socket.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(
            result["result"]["context"][TIER_CONTEXT_KEY],
            TIER_UNTRUSTED
        );
        socket.close(None).await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), worker)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }

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

    /// Claiming a trigger type hands the claimant every existing binding of that
    /// type and strands the ones registered after it, so the hook has to answer
    /// with an error, not a permissive object.
    #[test]
    fn the_trigger_type_hook_refuses_an_in_process_type_and_admits_a_registry_one() {
        let denied = reply(&invoke_frame(
            TRIGGER_TYPE_REGISTRATION_HOOK_ID,
            json!({
                "trigger_type_id": "http",
                "description": "route table capture",
                "context": { TIER_CONTEXT_KEY: TIER_UNTRUSTED },
            }),
        ));
        assert_eq!(
            denied["error"]["code"],
            json!("trigger_type_registration_denied")
        );
        assert!(
            denied.get("result").is_none(),
            "an object result is what the engine reads as ALLOW"
        );

        let allowed = reply(&invoke_frame(
            TRIGGER_TYPE_REGISTRATION_HOOK_ID,
            json!({
                "trigger_type_id": "state",
                "context": { TIER_CONTEXT_KEY: TIER_UNTRUSTED },
            }),
        ));
        assert_eq!(allowed["result"], json!({ "trigger_type_id": "state" }));

        let trusted = reply(&invoke_frame(
            TRIGGER_TYPE_REGISTRATION_HOOK_ID,
            json!({
                "trigger_type_id": "http",
                "context": { TIER_CONTEXT_KEY: TIER_TRUSTED },
            }),
        ));
        assert_eq!(trusted["result"], json!({ "trigger_type_id": "http" }));
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
