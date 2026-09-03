use agentos_http_adapter::TriggerBus;
use iii_sdk::errors::Error;
use iii_sdk::protocol::TriggerAction;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use subtle::ConstantTimeEq;

const MATRIX_MAX_LEN: usize = 4096;

/// How many handled transaction ids are remembered, oldest evicted first.
/// A homeserver retries a transaction only until it sees our 200, and sends
/// them linearised, so the id being retried is always among the most recent.
const SEEN_TRANSACTION_CAP: usize = 4096;

/// Name of the secret that authenticates the homeserver: the `hs_token` from
/// the application service registration file. The homeserver sends it as
/// `Authorization: Bearer <hs_token>` on every request it makes to us
/// (https://spec.matrix.org/latest/application-service-api/#authorization,
/// v1.4+; older homeservers send `?access_token=` instead). Distinct from
/// `MATRIX_TOKEN`, which authenticates OUR calls to the homeserver.
const HS_TOKEN_KEY: &str = "MATRIX_HS_TOKEN";

/// Prefix the application service registration `url` points at. The spec lets
/// the URL carry a path, and the homeserver appends the AS API paths to it, so
/// `url: http://host:3111/webhook/matrix` yields the routes registered below.
const ROUTE_PREFIX: &str = "webhook/matrix";

fn split_message(text: &str, max_len: usize) -> Vec<String> {
    if text.chars().count() <= max_len {
        return vec![text.to_string()];
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut remaining: &str = text;
    while !remaining.is_empty() {
        if remaining.chars().count() <= max_len {
            chunks.push(remaining.to_string());
            break;
        }
        let mut split_idx = remaining
            .char_indices()
            .take(max_len + 1)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(remaining.len());
        if let Some(nl) = remaining[..split_idx].rfind('\n')
            && nl >= max_len / 2
        {
            split_idx = nl;
        }
        chunks.push(remaining[..split_idx].to_string());
        remaining = &remaining[split_idx..];
    }
    chunks
}

fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// One header by case-insensitive name. The engine lowercases header names;
/// matching case-insensitively costs nothing and survives an engine change.
fn header<'a>(req: &'a Value, name: &str) -> Option<&'a str> {
    req.get("headers")?
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.as_str())
}

/// A Matrix error response: `{ "errcode": ..., "error": ... }` with the status
/// the homeserver expects.
fn matrix_error(status: u16, errcode: &str, error: &str) -> Value {
    json!({
        "status_code": status,
        "body": { "errcode": errcode, "error": error },
    })
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// Transactions already handled, by `txnId`.
///
/// The spec's transaction API (https://spec.matrix.org/latest/application-service-api/#pushing-events):
/// "Homeserver retries with the same transaction ID of T. […] If the AS had
/// processed these events already, it can NO-OP this request (and it knows if
/// it is the same events based on the transaction ID)", and homeservers "MUST
/// NOT alter (e.g. add more) events they were going to send within that
/// transaction ID on retries". So a repeated `txnId` is the same batch: it is
/// answered `200 {}` without being dispatched again. Bounded by
/// [`SEEN_TRANSACTION_CAP`].
#[derive(Default)]
struct TransactionLog {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

type SharedTransactionLog = Arc<Mutex<TransactionLog>>;

impl TransactionLog {
    /// Record `txn_id` as handled. `false` when it already was.
    fn first_sight(&mut self, txn_id: &str) -> bool {
        if self.seen.contains(txn_id) {
            return false;
        }
        while self.order.len() >= SEEN_TRANSACTION_CAP {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        self.seen.insert(txn_id.to_owned());
        self.order.push_back(txn_id.to_owned());
        true
    }
}

/// The transaction id of a `PUT …/transactions/{txnId}` call, as the adapter
/// delivers a path parameter: under `path_params` or scalarised at the top
/// level. `None` for the ping route.
fn transaction_id(req: &Value) -> Option<&str> {
    req.pointer("/path_params/txnId")
        .or_else(|| req.get("txnId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

async fn resolve_agent(iii: &dyn TriggerBus, channel: &str, channel_id: &str) -> String {
    let result = iii
        .trigger(TriggerRequest {
            function_id: "state::get".to_string(),
            payload: json!({
                "scope": "channel_agents",
                "key": format!("{channel}:{channel_id}"),
            }),
            action: None,
            timeout_ms: None,
        })
        .await;
    match result {
        Ok(v) => v
            .get("agentId")
            .and_then(|a| a.as_str())
            .unwrap_or("default")
            .to_string(),
        Err(_) => "default".to_string(),
    }
}

async fn get_secret(iii: &dyn TriggerBus, key: &str) -> String {
    let result = iii
        .trigger(TriggerRequest {
            function_id: "vault::get".to_string(),
            payload: json!({ "key": key }),
            action: None,
            timeout_ms: None,
        })
        .await;
    if let Ok(v) = result
        && let Some(value) = v.get("value").and_then(|s| s.as_str())
        && !value.is_empty()
    {
        return value.to_string();
    }
    std::env::var(key).unwrap_or_default()
}

/// The inbound verification secret at boot, or `None` when neither the vault
/// nor the environment has one. `main` registers the webhook route only on
/// `Some`: a route that cannot verify its caller is refused, not exposed.
/// (`vault::get` fails instantly with `function_not_found` while the vault
/// worker is still starting, so this falls through to the environment — the
/// path `agentos up` and `dev-up.sh` populate — without waiting.)
async fn startup_secret(iii: &dyn TriggerBus, key: &str) -> Option<String> {
    let value = get_secret(iii, key).await;
    (!value.is_empty()).then_some(value)
}

/// Check the homeserver's credential against our `hs_token`.
///
/// The spec (v1.4+) puts it in `Authorization: Bearer`; pre-v1.4 homeservers
/// send `?access_token=` instead, and may send both, in which case both must
/// match. Comparison is constant-time. Returns the Matrix error to answer with.
fn verify_hs_token(hs_token: &str, req: &Value) -> Result<(), Value> {
    if hs_token.is_empty() {
        return Err(matrix_error(
            503,
            "M_UNKNOWN",
            "MATRIX_HS_TOKEN not configured",
        ));
    }
    let bearer = header(req, "authorization")
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
        .filter(|token| !token.is_empty());
    let query = req
        .pointer("/query/access_token")
        .or_else(|| req.pointer("/query_params/access_token"))
        .and_then(|value| match value {
            Value::Array(values) => values.first().and_then(Value::as_str),
            other => other.as_str(),
        })
        .filter(|token| !token.is_empty());
    let presented: Vec<&str> = bearer.into_iter().chain(query).collect();
    if presented.is_empty() {
        return Err(matrix_error(401, "M_MISSING_TOKEN", "Missing hs_token"));
    }
    // Every presented token is compared; a mismatch anywhere is a refusal.
    let mut ok = true;
    for token in presented {
        ok &= constant_time_eq(token, hs_token);
    }
    if ok {
        Ok(())
    } else {
        Err(matrix_error(403, "M_FORBIDDEN", "Invalid hs_token"))
    }
}

async fn send_message(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    room_id: &str,
    text: &str,
) -> Result<(), Error> {
    let homeserver = get_secret(iii, "MATRIX_HOMESERVER").await;
    if homeserver.is_empty() {
        return Err(Error::Handler("MATRIX_HOMESERVER not configured".into()));
    }
    let token = get_secret(iii, "MATRIX_TOKEN").await;
    if token.is_empty() {
        return Err(Error::Handler("MATRIX_TOKEN not configured".into()));
    }
    let txn_base = uuid::Uuid::new_v4().to_string();
    let chunks = split_message(text, MATRIX_MAX_LEN);
    let encoded_room = url_encode(room_id);
    for (i, chunk) in chunks.iter().enumerate() {
        let url = format!(
            "{homeserver}/_matrix/client/v3/rooms/{encoded_room}/send/m.room.message/{txn_base}-{i}"
        );
        let res = client
            .put(&url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            // Use m.notice for bot-generated replies. Matrix clients and
            // well-behaved bots ignore m.notice events when deciding what to
            // process, which prevents the worker from ingesting its own
            // messages if the homeserver echoes them back to the webhook.
            .json(&json!({ "msgtype": "m.notice", "body": chunk }))
            .send()
            .await
            .map_err(|e| Error::Handler(e.to_string()))?;
        if !res.status().is_success() {
            return Err(Error::Handler(format!(
                "Matrix send failed: {}",
                res.status()
            )));
        }
    }
    Ok(())
}

/// Dispatch one `m.room.message` event to the agent and post the reply.
async fn handle_event(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    event: &Value,
) -> Result<(), Error> {
    if event.get("type").and_then(|t| t.as_str()) != Some("m.room.message") {
        return Ok(());
    }

    // Skip events authored as m.notice — those are bot-generated replies
    // (including ours) and processing them would create a feedback loop.
    let msgtype = event
        .get("content")
        .and_then(|c| c.get("msgtype"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if msgtype == "m.notice" {
        return Ok(());
    }

    let room_id = event
        .get("room_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let text = event
        .get("content")
        .and_then(|c| c.get("body"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let sender = event
        .get("sender")
        .and_then(|v| v.as_str())
        .map(String::from);

    if text.is_empty() {
        return Ok(());
    }

    let agent_id = resolve_agent(iii, "matrix", &room_id).await;

    let chat_response = iii
        .trigger(TriggerRequest {
            function_id: "agent::chat".to_string(),
            payload: json!({
                "agentId": agent_id,
                "message": text,
                "sessionId": format!("matrix:{room_id}"),
            }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let reply = chat_response
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();

    if !reply.is_empty() && !room_id.is_empty() {
        send_message(iii, client, &room_id, &reply).await?;
    }

    let _ = iii
        .trigger(TriggerRequest {
            function_id: "security::audit".to_string(),
            payload: json!({
                "type": "channel_message",
                "agentId": agent_id,
                "detail": { "channel": "matrix", "roomId": room_id, "sender": sender },
            }),
            action: Some(TriggerAction::Void),
            timeout_ms: None,
        })
        .await;

    Ok(())
}

/// The application service endpoints the homeserver calls
/// (https://spec.matrix.org/latest/application-service-api/):
///
/// * `PUT …/_matrix/app/v1/transactions/{txnId}` — a batch of events. Answered
///   `200 {}` after every event is handled; a repeated `txnId` (the
///   homeserver's retry of a batch we already handled) is answered `200 {}`
///   at once, see [`TransactionLog`]. (Also served at the pre-v1 path
///   `…/transactions/{txnId}`, which homeservers fall back to.)
/// * `POST …/_matrix/app/v1/ping` — the homeserver checking that the
///   connection and its `hs_token` work. Answered `200 {}`.
///
/// Every call is authenticated with the `hs_token` first; nothing in the body
/// is read before that. There is no body signature in this API — the bearer IS
/// the credential — so the engine's parsed body is used as is.
async fn webhook_handler(
    iii: &dyn TriggerBus,
    client: &reqwest::Client,
    handled: &SharedTransactionLog,
    input: Value,
) -> Result<Value, Error> {
    let hs_token = get_secret(iii, HS_TOKEN_KEY).await;
    if let Err(response) = verify_hs_token(&hs_token, &input) {
        if response["status_code"] != 503 {
            tracing::warn!(
                errcode = %response["body"]["errcode"],
                "matrix: homeserver credential rejected"
            );
        }
        return Ok(response);
    }

    let path = input.get("path").and_then(Value::as_str).unwrap_or("");
    if path.ends_with("/ping") {
        return Ok(json!({ "status_code": 200, "body": {} }));
    }

    // Recorded before dispatch, so a retry that overlaps a slow first attempt
    // is a no-op too. The homeserver only retries until it sees a 200.
    if let Some(txn_id) = transaction_id(&input)
        && !handled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .first_sight(txn_id)
    {
        tracing::info!(
            txn_id,
            "matrix: transaction already handled, acknowledging without dispatch"
        );
        return Ok(json!({ "status_code": 200, "body": {} }));
    }

    let body = input.get("body").cloned().unwrap_or(Value::Null);
    let events = body
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for event in &events {
        if let Err(e) = handle_event(iii, client, event).await {
            tracing::error!(error = %e, "matrix: failed to handle event");
        }
    }

    Ok(json!({ "status_code": 200, "body": {} }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());
    let client = reqwest::Client::new();

    let handled: SharedTransactionLog = Arc::new(Mutex::new(TransactionLog::default()));

    let iii_clone = iii.clone();
    let client_clone = client.clone();
    let handled_clone = handled.clone();
    iii.register_function(
        "channel::matrix::webhook",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            let client = client_clone.clone();
            let handled = handled_clone.clone();
            async move { webhook_handler(&iii, &client, &handled, input).await }
        })
        .description("Handle Matrix application service transactions"),
    );

    // The routes are registered only when the hs_token that authenticates the
    // homeserver exists. Without it every call would be refused anyway, and an
    // unverifiable route is not worth exposing. The handler re-reads the token
    // per request, so a rotation needs no restart.
    if startup_secret(&iii, HS_TOKEN_KEY).await.is_some() {
        agentos_http_adapter::register_http_trigger(
            &iii,
            "channel::matrix::webhook".to_string(),
            json!({ "http_method": "PUT", "api_path": "webhook/matrix/_matrix/app/v1/transactions/:txnId" }),
            None,
        )?;
        agentos_http_adapter::register_http_trigger(
            &iii,
            "channel::matrix::webhook".to_string(),
            json!({ "http_method": "PUT", "api_path": "webhook/matrix/transactions/:txnId" }),
            None,
        )?;
        agentos_http_adapter::register_http_trigger(
            &iii,
            "channel::matrix::webhook".to_string(),
            json!({ "http_method": "POST", "api_path": "webhook/matrix/_matrix/app/v1/ping" }),
            None,
        )?;
        tracing::info!(
            "matrix application service routes registered under /{ROUTE_PREFIX} (hs_token verified)"
        );
    } else {
        tracing::error!(
            "{HS_TOKEN_KEY} is not configured: the /{ROUTE_PREFIX} application service routes are \
             NOT registered. Set it to the hs_token from the registration file and restart."
        );
    }

    tracing::info!("channel-matrix worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_http_adapter::fake::FakeBus;

    const HS_TOKEN: &str = "312df522183efd404ec1cd22d2ffa4bbc76a8c1ccf541dd692eef281356bb74e";
    /// A transaction as a homeserver sends it: the `events` list of ClientEvents
    /// (https://spec.matrix.org/latest/application-service-api/#put_matrixappv1transactionstxnid).
    fn transaction() -> Value {
        json!({
            "events": [{
                "content": { "body": "hello agent", "msgtype": "m.text" },
                "event_id": "$143273582443PhrSn:example.org",
                "origin_server_ts": 1432735824653_u64,
                "room_id": "!636q39766251:example.com",
                "sender": "@example:example.org",
                "type": "m.room.message",
                "unsigned": { "age": 1234 }
            }]
        })
    }

    fn request(path: &str, method: &str, body: Value, authorization: Option<&str>) -> Value {
        let mut headers = json!({ "content-type": "application/json" });
        if let Some(authorization) = authorization {
            headers["authorization"] = json!(authorization);
        }
        json!({
            "method": method,
            "path": path,
            "path_params": { "txnId": "35" },
            "headers": headers,
            "body": body,
        })
    }

    fn fresh_log() -> SharedTransactionLog {
        Arc::new(Mutex::new(TransactionLog::default()))
    }

    fn bus_with_token(token: &str) -> FakeBus {
        let bus = FakeBus::new();
        let token = token.to_string();
        bus.on("vault::get", move |payload| {
            let key = payload["key"].as_str().unwrap_or_default();
            Ok(json!({ "value": if key == HS_TOKEN_KEY { token.clone() } else { String::new() } }))
        });
        bus.on_value("state::get", json!({ "agentId": "default" }));
        bus.on_value("agent::chat", json!({ "content": "" }));
        bus.on_value("security::audit", json!({}));
        bus
    }

    #[test]
    fn bearer_hs_token_verifies() {
        let req = json!({ "headers": { "authorization": format!("Bearer {HS_TOKEN}") } });
        assert!(verify_hs_token(HS_TOKEN, &req).is_ok());
        let upper = json!({ "headers": { "Authorization": format!("bearer {HS_TOKEN}") } });
        assert!(verify_hs_token(HS_TOKEN, &upper).is_ok());
    }

    #[test]
    fn legacy_query_token_verifies_and_must_agree_with_the_header() {
        let query_only = json!({ "query": { "access_token": HS_TOKEN } });
        assert!(verify_hs_token(HS_TOKEN, &query_only).is_ok());
        let both = json!({
            "headers": { "authorization": format!("Bearer {HS_TOKEN}") },
            "query": { "access_token": HS_TOKEN },
        });
        assert!(verify_hs_token(HS_TOKEN, &both).is_ok());
        let disagree = json!({
            "headers": { "authorization": format!("Bearer {HS_TOKEN}") },
            "query": { "access_token": "something-else" },
        });
        let err = verify_hs_token(HS_TOKEN, &disagree).unwrap_err();
        assert_eq!(err["status_code"], 403);
        assert_eq!(err["body"]["errcode"], "M_FORBIDDEN");
    }

    #[test]
    fn wrong_token_prefix_and_scheme_fail() {
        for authorization in [
            "Bearer wrong",
            &format!("Bearer {HS_TOKEN}x"),
            &format!("Bearer {}", &HS_TOKEN[..HS_TOKEN.len() - 1]),
            &format!("Basic {HS_TOKEN}"),
            HS_TOKEN,
        ] {
            let req = json!({ "headers": { "authorization": authorization } });
            let err = verify_hs_token(HS_TOKEN, &req).unwrap_err();
            assert!(
                err["status_code"] == 403 || err["status_code"] == 401,
                "{authorization}: {err}"
            );
        }
    }

    #[test]
    fn missing_token_is_401_and_missing_secret_is_503() {
        let err = verify_hs_token(HS_TOKEN, &json!({ "headers": {} })).unwrap_err();
        assert_eq!(err["status_code"], 401);
        assert_eq!(err["body"]["errcode"], "M_MISSING_TOKEN");

        let req = json!({ "headers": { "authorization": "Bearer " } });
        assert_eq!(verify_hs_token("", &req).unwrap_err()["status_code"], 503);
        assert_eq!(
            verify_hs_token(HS_TOKEN, &req).unwrap_err()["status_code"],
            401
        );
    }

    #[tokio::test]
    async fn an_authenticated_transaction_reaches_the_agent() {
        let bus = bus_with_token(HS_TOKEN);
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            &fresh_log(),
            request(
                "/webhook/matrix/_matrix/app/v1/transactions/:txnId",
                "PUT",
                transaction(),
                Some(&format!("Bearer {HS_TOKEN}")),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 200);
        assert_eq!(response["body"], json!({}));
        let chats = bus.calls_to("agent::chat");
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].payload["message"], "hello agent");
        assert_eq!(
            chats[0].payload["sessionId"],
            "matrix:!636q39766251:example.com"
        );
    }

    #[tokio::test]
    async fn a_forged_transaction_never_reaches_the_agent() {
        let bus = bus_with_token(HS_TOKEN);
        for authorization in [None, Some("Bearer not-the-hs-token")] {
            let response = webhook_handler(
                &bus,
                &reqwest::Client::new(),
                &fresh_log(),
                request(
                    "/webhook/matrix/_matrix/app/v1/transactions/:txnId",
                    "PUT",
                    transaction(),
                    authorization,
                ),
            )
            .await
            .unwrap();
            assert!(response["status_code"] == 401 || response["status_code"] == 403);
            assert!(response["body"]["errcode"].is_string());
        }
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert_eq!(bus.call_count("state::get"), 0);
    }

    #[tokio::test]
    async fn ping_is_answered_only_when_authenticated() {
        let bus = bus_with_token(HS_TOKEN);
        let ping = json!({ "transaction_id": "mautrix-go_1683636478256400935_123" });
        let ok = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            &fresh_log(),
            request(
                "/webhook/matrix/_matrix/app/v1/ping",
                "POST",
                ping.clone(),
                Some(&format!("Bearer {HS_TOKEN}")),
            ),
        )
        .await
        .unwrap();
        assert_eq!(ok["status_code"], 200);
        assert_eq!(ok["body"], json!({}));

        let forbidden = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            &fresh_log(),
            request(
                "/webhook/matrix/_matrix/app/v1/ping",
                "POST",
                ping,
                Some("Bearer nope"),
            ),
        )
        .await
        .unwrap();
        assert_eq!(forbidden["status_code"], 403);
        assert_eq!(forbidden["body"]["errcode"], "M_FORBIDDEN");
    }

    #[tokio::test]
    async fn notices_and_non_message_events_are_ignored() {
        let bus = bus_with_token(HS_TOKEN);
        let body = json!({
            "events": [
                { "type": "m.room.member", "room_id": "!r:x", "sender": "@a:x", "content": { "membership": "join" } },
                { "type": "m.room.message", "room_id": "!r:x", "sender": "@bot:x", "content": { "msgtype": "m.notice", "body": "our own reply" } },
                { "type": "m.room.message", "room_id": "!r:x", "sender": "@a:x", "content": { "msgtype": "m.text", "body": "" } }
            ]
        });
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            &fresh_log(),
            request(
                "/webhook/matrix/_matrix/app/v1/transactions/:txnId",
                "PUT",
                body,
                Some(&format!("Bearer {HS_TOKEN}")),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 200);
        assert_eq!(bus.call_count("agent::chat"), 0);
    }

    #[tokio::test]
    async fn a_missing_hs_token_refuses_the_call_and_the_routes() {
        let bus = bus_with_token("");
        let response = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            &fresh_log(),
            request(
                "/webhook/matrix/_matrix/app/v1/transactions/:txnId",
                "PUT",
                transaction(),
                Some("Bearer anything"),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response["status_code"], 503);
        assert_eq!(bus.call_count("agent::chat"), 0);
        assert_eq!(startup_secret(&bus, HS_TOKEN_KEY).await, None);
        assert_eq!(
            startup_secret(&bus_with_token(HS_TOKEN), HS_TOKEN_KEY)
                .await
                .as_deref(),
            Some(HS_TOKEN)
        );
    }

    #[test]
    fn url_encode_passes_unreserved() {
        assert_eq!(url_encode("abcXYZ123-_.~"), "abcXYZ123-_.~");
    }

    #[test]
    fn url_encode_escapes_room_id() {
        assert_eq!(url_encode("!room1:matrix.org"), "%21room1%3Amatrix.org");
    }

    #[test]
    fn split_short_returns_single() {
        assert_eq!(split_message("hi", 4096), vec!["hi".to_string()]);
    }

    #[test]
    fn split_long_chunks() {
        let text = "a".repeat(5000);
        let chunks = split_message(&text, 4096);
        assert!(chunks.len() >= 2);
    }
    #[tokio::test]
    async fn a_retried_transaction_is_acknowledged_without_a_second_dispatch() {
        let bus = bus_with_token(HS_TOKEN);
        let handled = fresh_log();
        let delivery = request(
            "/webhook/matrix/_matrix/app/v1/transactions/:txnId",
            "PUT",
            transaction(),
            Some(&format!("Bearer {HS_TOKEN}")),
        );

        // A forged attempt with the same txnId must not poison the log: the
        // credential is checked first, so nothing unauthenticated is recorded.
        let forged = webhook_handler(
            &bus,
            &reqwest::Client::new(),
            &handled,
            request(
                "/webhook/matrix/_matrix/app/v1/transactions/:txnId",
                "PUT",
                transaction(),
                Some("Bearer not-the-hs-token"),
            ),
        )
        .await
        .unwrap();
        assert_eq!(forged["status_code"], 403);
        assert_eq!(handled.lock().unwrap().order.len(), 0);

        let first = webhook_handler(&bus, &reqwest::Client::new(), &handled, delivery.clone())
            .await
            .unwrap();
        assert_eq!(first["status_code"], 200);
        assert_eq!(bus.call_count("agent::chat"), 1);

        // "Homeserver retries with the same transaction ID of T": same 200, no
        // second dispatch.
        let retry = webhook_handler(&bus, &reqwest::Client::new(), &handled, delivery)
            .await
            .unwrap();
        assert_eq!(retry["status_code"], 200);
        assert_eq!(retry["body"], json!({}));
        assert_eq!(bus.call_count("agent::chat"), 1);

        // A new transaction id is a new batch.
        let mut next = request(
            "/webhook/matrix/_matrix/app/v1/transactions/:txnId",
            "PUT",
            transaction(),
            Some(&format!("Bearer {HS_TOKEN}")),
        );
        next["path_params"]["txnId"] = json!("36");
        let second = webhook_handler(&bus, &reqwest::Client::new(), &handled, next)
            .await
            .unwrap();
        assert_eq!(second["status_code"], 200);
        assert_eq!(bus.call_count("agent::chat"), 2);
    }

    #[test]
    fn transaction_id_is_read_the_way_the_adapter_delivers_a_path_parameter() {
        assert_eq!(
            transaction_id(&json!({ "path_params": { "txnId": "35" } })),
            Some("35")
        );
        assert_eq!(transaction_id(&json!({ "txnId": "35" })), Some("35"));
        assert_eq!(
            transaction_id(&json!({ "path_params": { "txnId": "" } })),
            None
        );
        assert_eq!(
            transaction_id(&json!({ "path": "/webhook/matrix/_matrix/app/v1/ping" })),
            None
        );
    }

    #[test]
    fn transaction_log_is_bounded_and_evicts_the_oldest() {
        let mut log = TransactionLog::default();
        assert!(log.first_sight("t-0"));
        assert!(!log.first_sight("t-0"));
        for i in 1..SEEN_TRANSACTION_CAP {
            assert!(log.first_sight(&format!("t-{i}")));
        }
        assert_eq!(log.order.len(), SEEN_TRANSACTION_CAP);
        assert!(!log.first_sight("t-0"), "still full, still remembered");
        assert!(log.first_sight("t-overflow"));
        assert_eq!(log.order.len(), SEEN_TRANSACTION_CAP);
        assert!(
            log.first_sight("t-0"),
            "the oldest id was evicted to make room"
        );
        assert_eq!(log.order.len(), SEEN_TRANSACTION_CAP);
        assert!(
            log.first_sight("t-1"),
            "re-recording t-0 evicted the next oldest"
        );
        assert!(
            !log.first_sight("t-3"),
            "everything younger is still remembered"
        );
        assert!(!log.first_sight("t-overflow"));
    }
}
