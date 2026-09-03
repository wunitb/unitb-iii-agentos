use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use agentos_http_adapter::TriggerBus;
use agentos_http_adapter::principal::{self, Principal};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use iii_sdk::errors::Error;
use iii_sdk::{RegisterFunction, protocol::TriggerRequest, register_worker};
use rand::RngExt;
use scrypt::{Params, scrypt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const DEFAULT_AUTO_LOCK_MS: u64 = 30 * 60 * 1000;
const MIN_PASSWORD_LEN: usize = 8;
const SALT_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultEntry {
    key: String,
    iv: String,
    ciphertext: String,
    tag: String,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultMeta {
    salt: String,
    created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    rotated_at: Option<i64>,
}

#[derive(Default)]
struct VaultState {
    crypto_key: Option<Vec<u8>>,
    salt_b64: Option<String>,
    auto_lock_ms: u64,
    last_activity: Option<Instant>,
}

impl VaultState {
    fn unlocked(&self) -> bool {
        self.crypto_key.is_some()
    }

    fn check_auto_lock(&mut self) {
        if let (Some(last), key) = (self.last_activity, self.crypto_key.as_ref())
            && key.is_some()
            && last.elapsed() >= Duration::from_millis(self.auto_lock_ms)
        {
            self.crypto_key = None;
            self.last_activity = None;
        }
    }

    fn touch(&mut self) {
        self.last_activity = Some(Instant::now());
    }

    #[allow(dead_code)]
    fn lock(&mut self) {
        self.crypto_key = None;
        self.last_activity = None;
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    rand::rng().fill(&mut buf);
    buf
}

fn derive_key(password: &str, salt: &[u8]) -> Result<Vec<u8>, Error> {
    let params =
        Params::new(15, 8, 1, 32).map_err(|e| Error::Handler(format!("scrypt params: {e}")))?;
    let mut out = vec![0u8; 32];
    scrypt(password.as_bytes(), salt, &params, &mut out)
        .map_err(|e| Error::Handler(format!("scrypt: {e}")))?;
    Ok(out)
}

fn encrypt(key: &[u8], plaintext: &str) -> Result<(String, String, String), Error> {
    let cipher_key = Key::<Aes256Gcm>::from_slice(key);
    let cipher = Aes256Gcm::new(cipher_key);
    let nonce_bytes = random_bytes(NONCE_LEN);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let combined = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext.as_bytes(),
                aad: &[],
            },
        )
        .map_err(|e| Error::Handler(format!("encrypt failed: {e}")))?;

    if combined.len() < TAG_LEN {
        return Err(Error::Handler("encrypted output too short".into()));
    }
    let split = combined.len() - TAG_LEN;
    let ciphertext = &combined[..split];
    let tag = &combined[split..];

    Ok((
        B64.encode(&nonce_bytes),
        B64.encode(ciphertext),
        B64.encode(tag),
    ))
}

fn decrypt(key: &[u8], iv_b64: &str, ciphertext_b64: &str, tag_b64: &str) -> Result<String, Error> {
    let cipher_key = Key::<Aes256Gcm>::from_slice(key);
    let cipher = Aes256Gcm::new(cipher_key);

    let iv = B64
        .decode(iv_b64)
        .map_err(|e| Error::Handler(format!("iv decode: {e}")))?;
    let ciphertext = B64
        .decode(ciphertext_b64)
        .map_err(|e| Error::Handler(format!("ciphertext decode: {e}")))?;
    let tag = B64
        .decode(tag_b64)
        .map_err(|e| Error::Handler(format!("tag decode: {e}")))?;

    if iv.len() != NONCE_LEN {
        return Err(Error::Handler("invalid iv length".into()));
    }
    if tag.len() != TAG_LEN {
        return Err(Error::Handler("invalid tag length".into()));
    }

    let mut combined = Vec::with_capacity(ciphertext.len() + tag.len());
    combined.extend_from_slice(&ciphertext);
    combined.extend_from_slice(&tag);

    let nonce = Nonce::from_slice(&iv);
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &combined,
                aad: &[],
            },
        )
        .map_err(|e| Error::Handler(format!("decrypt failed: {e}")))?;

    String::from_utf8(plaintext).map_err(|e| Error::Handler(format!("utf8: {e}")))
}

/// The bus handle vault handlers take: the engine client in production, a
/// `FakeBus` in tests. `Arc` because the audit trail is written from a spawned
/// task and a `&dyn` cannot outlive the handler.
type Bus = Arc<dyn TriggerBus>;

/// Who this call is from (contract T1), against the one bearer
/// `agentos_bus_auth` owns. Constant-time; no second credential is read here.
fn principal_of(input: &Value) -> Result<Principal, Error> {
    let expected = agentos_bus_auth::policy::expected_api_key();
    Ok(principal::resolve(input, expected.as_deref())?)
}

/// The operator gate for key management.
///
/// `vault::init`, `vault::rotate`, `vault::backup` and `vault::restore` handle
/// the master password and the whole store; no capability can hand them to an
/// agent, so an agent principal is refused here even when it holds `vault::*`
/// grants. Unconditional: a bus caller never carries a `headers` object, and
/// gating on its presence once made every read reachable from the bus.
fn require_auth(input: &Value) -> Result<(), Error> {
    match principal_of(input) {
        Ok(Principal::Operator) => Ok(()),
        _ => Err(Error::Handler("Unauthorized".into())),
    }
}

/// `security::check_capability` answers `{"allowed": true}` or an error;
/// anything else, including an unreachable reader, is a denial.
fn capability_denial(result: &Result<Value, Error>) -> Option<String> {
    match result {
        Ok(value) if value.get("allowed").and_then(Value::as_bool) == Some(true) => None,
        Ok(value) => Some(
            value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("not granted")
                .to_string(),
        ),
        Err(error) => Some(error.to_string()),
    }
}

/// Access rule for the credential operations (`vault::get/set/list/delete`).
///
/// The vault has ONE namespace, the operator's: there is no per-agent store and
/// this WP does not invent one. So:
/// * the operator (bearer) may use it;
/// * an agent principal may use exactly the function its capability document
///   grants by exact id (`vault::*` is a deny-by-default family in contract I1,
///   so no wildcard reaches it), checked through `security::check_capability`;
/// * a payload `agentId` that is not the principal's own agent is refused
///   outright — before this rule it was silently IGNORED and the global store
///   answered a request that asked for somebody's private one.
async fn authorize(iii: &Bus, input: &Value, function_id: &str) -> Result<Principal, Error> {
    let principal = principal_of(input).map_err(|_| Error::Handler("Unauthorized".into()))?;
    let body = body_or_self(input);
    let requested = principal::requested_agent(input).or_else(|| principal::requested_agent(&body));
    if let Some(target) = requested
        && Some(target) != principal.agent_id()
    {
        return Err(Error::Handler(format!(
            "the vault has no per-agent namespace: agentId {target} cannot be honoured for {principal}"
        )));
    }
    if let Principal::Agent(agent) = &principal {
        let result = iii
            .trigger(TriggerRequest {
                function_id: "security::check_capability".to_string(),
                payload: json!({ "agentId": agent, "resource": function_id }),
                action: None,
                timeout_ms: None,
            })
            .await;
        if let Some(reason) = capability_denial(&result) {
            return Err(Error::Handler(format!(
                "agent {agent} is not granted {function_id}: {reason}"
            )));
        }
    }
    Ok(principal)
}

fn body_or_self(input: &Value) -> Value {
    let Some(mut body) = input.get("body").cloned() else {
        return input.clone();
    };

    if let (Some(body), Some(key)) = (body.as_object_mut(), input.get("key")) {
        body.entry("key".to_string()).or_insert_with(|| key.clone());
    }

    body
}

async fn state_get(iii: &Bus, scope: &str, key: &str) -> Option<Value> {
    iii.trigger(TriggerRequest {
        function_id: "state::get".to_string(),
        payload: json!({ "scope": scope, "key": key }),
        action: None,
        timeout_ms: None,
    })
    .await
    .ok()
}

async fn state_set(iii: &Bus, scope: &str, key: &str, value: Value) -> Result<(), Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({ "scope": scope, "key": key, "value": value }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;
    Ok(())
}

async fn state_delete(iii: &Bus, scope: &str, key: &str) -> Result<(), Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::delete".to_string(),
        payload: json!({ "scope": scope, "key": key }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;
    Ok(())
}

async fn state_list(iii: &Bus, scope: &str) -> Vec<Value> {
    iii.trigger(TriggerRequest {
        function_id: "state::list".to_string(),
        payload: json!({ "scope": scope }),
        action: None,
        timeout_ms: None,
    })
    .await
    .ok()
    .and_then(|v| v.as_array().cloned())
    .unwrap_or_default()
}

/// The stored credentials of a `state::list` over a vault scope, as
/// `(key, entry)` pairs, minus `__meta` and anything without a ciphertext.
///
/// The engine answers `state::list` with the BARE stored values (verified on
/// iii 0.22.1; see `agentos_http_adapter::state`), and a `VaultEntry` carries
/// its own `key`, so that is where the key is read from. The previous reader
/// expected a `{key, value}` envelope the engine never sends: `vault::list`
/// answered `[]` for a full store, and `vault::rotate` therefore re-keyed the
/// store with ZERO credentials re-encrypted, orphaning every secret behind the
/// old salt. A `{key, value}` envelope is still tolerated.
fn stored_credentials(entries: Vec<Value>) -> Vec<(String, Value)> {
    entries
        .into_iter()
        .filter_map(|entry| {
            let value = agentos_http_adapter::state::value_of(&entry).clone();
            let key = value
                .get("key")
                .and_then(Value::as_str)
                .filter(|key| !key.is_empty() && *key != "__meta")?
                .to_string();
            value
                .get("ciphertext")
                .and_then(Value::as_str)
                .filter(|ciphertext| !ciphertext.is_empty())?;
            Some((key, value))
        })
        .collect()
}

fn audit_void(iii: &Bus, audit_type: &str, detail: Value) {
    let payload = json!({ "type": audit_type, "detail": detail });
    let iii = Arc::clone(iii);
    tokio::spawn(async move {
        let _ = iii
            .trigger(TriggerRequest {
                function_id: "security::audit".to_string(),
                payload,
                action: None,
                timeout_ms: None,
            })
            .await;
    });
}

type SharedState = Arc<Mutex<VaultState>>;

async fn vault_init(state: SharedState, iii: &Bus, input: Value) -> Result<Value, Error> {
    require_auth(&input)?;
    let body = body_or_self(&input);
    let password = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
    if password.len() < MIN_PASSWORD_LEN {
        return Err(Error::Handler(format!(
            "Password must be at least {} characters",
            MIN_PASSWORD_LEN
        )));
    }

    let mut st = state.lock().await;

    if let Some(mins) = body.get("autoLockMinutes").and_then(|v| v.as_u64()) {
        st.auto_lock_ms = mins * 60_000;
    } else if st.auto_lock_ms == 0 {
        st.auto_lock_ms = DEFAULT_AUTO_LOCK_MS;
    }

    let existing = state_get(iii, "vault", "__meta").await;
    let salt: Vec<u8> = if let Some(meta) = existing
        .as_ref()
        .and_then(|v| v.get("salt"))
        .and_then(|v| v.as_str())
    {
        B64.decode(meta)
            .map_err(|e| Error::Handler(format!("salt decode: {e}")))?
    } else {
        let new_salt = random_bytes(SALT_LEN);
        let meta = VaultMeta {
            salt: B64.encode(&new_salt),
            created_at: now_ms(),
            rotated_at: None,
        };
        state_set(
            iii,
            "vault",
            "__meta",
            serde_json::to_value(&meta).map_err(|e| Error::Handler(e.to_string()))?,
        )
        .await?;
        new_salt
    };

    let key = derive_key(password, &salt)?;
    st.crypto_key = Some(key);
    st.salt_b64 = Some(B64.encode(&salt));
    st.touch();

    audit_void(
        iii,
        "vault_unlocked",
        json!({ "autoLockMs": st.auto_lock_ms }),
    );

    Ok(json!({
        "unlocked": true,
        "autoLockMinutes": st.auto_lock_ms / 60_000,
    }))
}

async fn vault_set(state: SharedState, iii: &Bus, input: Value) -> Result<Value, Error> {
    let principal = authorize(iii, &input, "vault::set").await?;
    let body = body_or_self(&input);
    let key = body
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let value = body
        .get("value")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut st = state.lock().await;
    st.check_auto_lock();
    let crypto_key = st
        .crypto_key
        .clone()
        .ok_or_else(|| Error::Handler("Vault is locked. Call vault::init first.".into()))?;

    if key.is_empty() || key.starts_with("__") {
        return Err(Error::Handler("Invalid key".into()));
    }

    st.touch();
    // `st` is held across the state calls so no credential read or write
    // interleaves with a rotation (see `vault_rotate`).

    let (iv, ciphertext, tag) = encrypt(&crypto_key, &value)?;
    let now = now_ms();

    let existing = state_get(iii, "vault", &key).await;
    let created_at = existing
        .as_ref()
        .and_then(|v| v.get("createdAt"))
        .and_then(|v| v.as_i64())
        .unwrap_or(now);

    let entry = VaultEntry {
        key: key.clone(),
        iv,
        ciphertext,
        tag,
        created_at,
        updated_at: now,
    };

    let value = serde_json::to_value(&entry).map_err(|e| Error::Handler(e.to_string()))?;
    state_set(iii, "vault", &key, value).await?;

    audit_void(
        iii,
        "vault_set",
        json!({ "key": &key, "principal": principal.to_string() }),
    );

    Ok(json!({
        "stored": true,
        "key": key,
        "updatedAt": now,
    }))
}

async fn vault_get(state: SharedState, iii: &Bus, input: Value) -> Result<Value, Error> {
    // Unconditional: a bus caller never carries a `headers` object, so gating the
    // check on its presence made every plaintext read reachable from the
    // unauthenticated engine bus. Matches vault::init/set/delete/rotate.
    let principal = authorize(iii, &input, "vault::get").await?;
    let body = body_or_self(&input);
    let key = body
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut st = state.lock().await;
    st.check_auto_lock();
    let crypto_key = st
        .crypto_key
        .clone()
        .ok_or_else(|| Error::Handler("Vault is locked. Call vault::init first.".into()))?;
    st.touch();
    // `st` is held across the state calls so no credential read or write
    // interleaves with a rotation (see `vault_rotate`).

    let entry = state_get(iii, "vault", &key)
        .await
        .filter(|v| {
            v.get("ciphertext")
                .and_then(|c| c.as_str())
                .is_some_and(|s| !s.is_empty())
        })
        .ok_or_else(|| Error::Handler(format!("Credential not found: {key}")))?;

    let iv = entry.get("iv").and_then(|v| v.as_str()).unwrap_or("");
    let ciphertext = entry
        .get("ciphertext")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let tag = entry.get("tag").and_then(|v| v.as_str()).unwrap_or("");
    let plaintext = decrypt(&crypto_key, iv, ciphertext, tag)?;

    audit_void(
        iii,
        "vault_get",
        json!({ "key": &key, "principal": principal.to_string() }),
    );

    Ok(json!({
        "key": key,
        "value": plaintext,
        "createdAt": entry.get("createdAt"),
        "updatedAt": entry.get("updatedAt"),
    }))
}

async fn vault_list(state: SharedState, iii: &Bus, input: Value) -> Result<Value, Error> {
    // Unconditional, for the same reason as vault_get: key names are themselves
    // sensitive and the bus is unauthenticated.
    authorize(iii, &input, "vault::list").await?;
    let mut st = state.lock().await;
    st.check_auto_lock();
    if !st.unlocked() {
        return Err(Error::Handler(
            "Vault is locked. Call vault::init first.".into(),
        ));
    }
    st.touch();
    // `st` is held across the state calls so no credential read or write
    // interleaves with a rotation (see `vault_rotate`).

    let keys: Vec<Value> = stored_credentials(state_list(iii, "vault").await)
        .into_iter()
        .map(|(key, v)| {
            json!({
                "key": key,
                "createdAt": v.get("createdAt"),
                "updatedAt": v.get("updatedAt"),
            })
        })
        .collect();

    let count = keys.len();
    Ok(json!({
        "keys": keys,
        "count": count,
    }))
}

async fn vault_delete(state: SharedState, iii: &Bus, input: Value) -> Result<Value, Error> {
    let principal = authorize(iii, &input, "vault::delete").await?;
    let body = body_or_self(&input);
    let key = body
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut st = state.lock().await;
    st.check_auto_lock();
    if !st.unlocked() {
        return Err(Error::Handler(
            "Vault is locked. Call vault::init first.".into(),
        ));
    }
    st.touch();
    // `st` is held across the state calls so no credential read or write
    // interleaves with a rotation (see `vault_rotate`).

    if key == "__meta" {
        return Err(Error::Handler("Cannot delete vault metadata".into()));
    }

    state_delete(iii, "vault", &key).await?;
    audit_void(
        iii,
        "vault_delete",
        json!({ "key": &key, "principal": principal.to_string() }),
    );

    Ok(json!({
        "deleted": true,
        "key": key,
    }))
}

/// Put the re-keyed credentials (and `__meta`, defensively: it is written last,
/// so a failure there may or may not have applied) back from `vault_backup`
/// after a failed rotation. Answers the entries that could NOT be restored,
/// with the error for each; an empty list means the store is what it was.
async fn roll_back_rotation(iii: &Bus, written: &[String]) -> Vec<(String, String)> {
    let mut unrestored = Vec::new();
    let keys = written
        .iter()
        .map(String::as_str)
        .chain(std::iter::once("__meta"));
    for key in keys {
        let outcome = match state_get(iii, "vault_backup", key).await {
            Some(backup) => state_set(iii, "vault", key, backup).await,
            None => Err(Error::Handler("no backup copy".into())),
        };
        if let Err(error) = outcome {
            tracing::error!(%key, %error, "vault rotation rollback failed for an entry");
            unrestored.push((key.to_string(), error.to_string()));
        }
    }
    unrestored
}

/// Re-key every credential under a new password.
///
/// The vault mutex is held from the first read to the in-memory key switch.
/// Rotation is N sequential `state::set` calls, and the engine offers no
/// transaction across them; a `vault::set` that ran in between would encrypt
/// with the OLD key and be orphaned under the new `__meta` (unreadable after
/// the switch). Every credential operation takes the same lock, so holding it
/// here serialises them behind the rotation instead. The cost is scrypt and
/// the state writes under the lock, which the vault's one-tenant design can
/// afford.
///
/// A failed write rolls the store back from `vault_backup`; a rollback that
/// itself fails is REPORTED in the error and the audit entry (with the keys
/// still to restore), never swallowed — the operator's recovery is
/// `vault::restore {password: <current>}`, which reads the same backup.
async fn vault_rotate(state: SharedState, iii: &Bus, input: Value) -> Result<Value, Error> {
    require_auth(&input)?;
    let body = body_or_self(&input);
    let current_password = body
        .get("currentPassword")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let new_password = body
        .get("newPassword")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut st = state.lock().await;
    st.check_auto_lock();
    if !st.unlocked() {
        return Err(Error::Handler(
            "Vault is locked. Call vault::init first.".into(),
        ));
    }
    if new_password.len() < MIN_PASSWORD_LEN {
        return Err(Error::Handler(format!(
            "New password must be at least {} characters",
            MIN_PASSWORD_LEN
        )));
    }
    // `st` stays locked until the key switch below (see the doc comment).

    let meta = state_get(iii, "vault", "__meta")
        .await
        .ok_or_else(|| Error::Handler("vault metadata missing".into()))?;
    let old_salt_b64 = meta
        .get("salt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Handler("vault salt missing".into()))?;
    let old_salt = B64
        .decode(old_salt_b64)
        .map_err(|e| Error::Handler(format!("salt decode: {e}")))?;
    let old_key = derive_key(&current_password, &old_salt)?;

    let credentials = stored_credentials(state_list(iii, "vault").await);

    state_set(iii, "vault_backup", "__meta", meta.clone()).await?;
    for (k, v) in &credentials {
        state_set(iii, "vault_backup", k, v.clone()).await?;
    }

    let new_salt = random_bytes(SALT_LEN);
    let new_key = derive_key(&new_password, &new_salt)?;

    let mut updates: Vec<(String, Value)> = Vec::new();
    for (k, v) in &credentials {
        let k = k.clone();

        let iv = v.get("iv").and_then(|v| v.as_str()).unwrap_or("");
        let ct = v.get("ciphertext").and_then(|v| v.as_str()).unwrap_or("");
        let tag = v.get("tag").and_then(|v| v.as_str()).unwrap_or("");
        let plaintext = decrypt(&old_key, iv, ct, tag)?;
        let (new_iv, new_ct, new_tag) = encrypt(&new_key, &plaintext)?;

        let mut new_value = v.clone();
        if let Some(obj) = new_value.as_object_mut() {
            obj.insert("iv".into(), json!(new_iv));
            obj.insert("ciphertext".into(), json!(new_ct));
            obj.insert("tag".into(), json!(new_tag));
            obj.insert("updatedAt".into(), json!(now_ms()));
        }
        updates.push((k, new_value));
    }

    // Written in order; on a failure only the entries actually written (plus
    // `__meta`, defensively) are rolled back, so the report names what changed.
    let mut written: Vec<String> = Vec::with_capacity(updates.len());
    let mut rotation_result: Result<(), Error> = Ok(());
    for (k, v) in &updates {
        if let Err(error) = state_set(iii, "vault", k, v.clone()).await {
            rotation_result = Err(error);
            break;
        }
        written.push(k.clone());
    }
    if rotation_result.is_ok() {
        let new_meta = json!({
            "salt": B64.encode(&new_salt),
            "createdAt": meta.get("createdAt").and_then(|v| v.as_i64()).unwrap_or(now_ms()),
            "rotatedAt": now_ms(),
        });
        rotation_result = state_set(iii, "vault", "__meta", new_meta).await;
    }

    if let Err(err) = rotation_result {
        let unrestored = roll_back_rotation(iii, &written).await;
        let rolled_back = unrestored.is_empty();
        audit_void(
            iii,
            "vault_rotation_failed",
            json!({
                "error": err.to_string(),
                "rolledBack": rolled_back,
                "unrestored": unrestored.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
            }),
        );
        if rolled_back {
            return Err(Error::Handler(format!(
                "Vault rotation failed, rolled back: {err}"
            )));
        }
        let detail = unrestored
            .iter()
            .map(|(key, error)| format!("{key} ({error})"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Error::Handler(format!(
            "Vault rotation failed: {err}; rollback incomplete, still to restore from vault_backup: \
             {detail}. Run vault::restore with the current password."
        )));
    }

    st.crypto_key = Some(new_key);
    st.salt_b64 = Some(B64.encode(&new_salt));
    st.touch();

    audit_void(
        iii,
        "vault_rotated",
        json!({ "credentialsRotated": updates.len() }),
    );

    Ok(json!({
        "rotated": updates.len(),
        "success": true,
    }))
}

async fn vault_backup(state: SharedState, iii: &Bus, input: Value) -> Result<Value, Error> {
    require_auth(&input)?;

    let mut st = state.lock().await;
    st.check_auto_lock();
    if !st.unlocked() {
        return Err(Error::Handler(
            "Vault is locked. Call vault::init first.".into(),
        ));
    }
    st.touch();
    // `st` is held across the state calls so no credential read or write
    // interleaves with a rotation (see `vault_rotate`).

    let meta = state_get(iii, "vault", "__meta")
        .await
        .ok_or_else(|| Error::Handler("vault metadata missing".into()))?;

    let credentials = stored_credentials(state_list(iii, "vault").await);

    state_set(iii, "vault_backup", "__meta", meta).await?;
    for (k, v) in &credentials {
        state_set(iii, "vault_backup", k, v.clone()).await?;
    }

    audit_void(
        iii,
        "vault_backup_created",
        json!({ "credentialsCount": credentials.len() }),
    );

    Ok(json!({
        "backedUp": credentials.len(),
        "success": true,
    }))
}

async fn vault_restore(state: SharedState, iii: &Bus, input: Value) -> Result<Value, Error> {
    require_auth(&input)?;
    let body = body_or_self(&input);
    let password = body
        .get("password")
        .and_then(|v| v.as_str())
        .map(String::from);

    // Held across the whole restore: it must not interleave with a rotation
    // or a credential write (see `vault_rotate`).
    let mut st = state.lock().await;

    let backup_meta = state_get(iii, "vault_backup", "__meta")
        .await
        .ok_or_else(|| Error::Handler("No vault backup found".into()))?;

    let credentials = stored_credentials(state_list(iii, "vault_backup").await);

    state_set(iii, "vault", "__meta", backup_meta.clone()).await?;
    for (k, v) in &credentials {
        state_set(iii, "vault", k, v.clone()).await?;
    }

    if let Some(pw) = password {
        if pw.len() < MIN_PASSWORD_LEN {
            return Err(Error::Handler(format!(
                "Password must be at least {} characters",
                MIN_PASSWORD_LEN
            )));
        }
        let salt_b64 = backup_meta
            .get("salt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Handler("backup salt missing".into()))?;
        let salt = B64
            .decode(salt_b64)
            .map_err(|e| Error::Handler(format!("salt decode: {e}")))?;
        let key = derive_key(&pw, &salt)?;

        st.crypto_key = Some(key);
        st.salt_b64 = Some(salt_b64.to_string());
        if st.auto_lock_ms == 0 {
            st.auto_lock_ms = DEFAULT_AUTO_LOCK_MS;
        }
        st.touch();
    }

    audit_void(
        iii,
        "vault_restored",
        json!({ "credentialsCount": credentials.len() }),
    );

    Ok(json!({
        "restored": credentials.len(),
        "success": true,
    }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());
    let bus: Bus = Arc::new(iii.clone());

    let state: SharedState = Arc::new(Mutex::new(VaultState {
        auto_lock_ms: DEFAULT_AUTO_LOCK_MS,
        ..Default::default()
    }));

    let s = state.clone();
    let i = Arc::clone(&bus);
    iii.register_function(
        "vault::init",
        RegisterFunction::new_async(move |input: Value| {
            let s = s.clone();
            let i = i.clone();
            async move { vault_init(s, &i, input).await }
        })
        .description("Initialize vault with master password"),
    );

    let s = state.clone();
    let i = Arc::clone(&bus);
    iii.register_function(
        "vault::set",
        RegisterFunction::new_async(move |input: Value| {
            let s = s.clone();
            let i = i.clone();
            async move { vault_set(s, &i, input).await }
        })
        .description("Store an encrypted credential"),
    );

    let s = state.clone();
    let i = Arc::clone(&bus);
    iii.register_function(
        "vault::get",
        RegisterFunction::new_async(move |input: Value| {
            let s = s.clone();
            let i = i.clone();
            async move { vault_get(s, &i, input).await }
        })
        .description("Retrieve and decrypt a credential"),
    );

    let s = state.clone();
    let i = Arc::clone(&bus);
    iii.register_function(
        "vault::list",
        RegisterFunction::new_async(move |input: Value| {
            let s = s.clone();
            let i = i.clone();
            async move { vault_list(s, &i, input).await }
        })
        .description("List stored credential keys without values"),
    );

    let s = state.clone();
    let i = Arc::clone(&bus);
    iii.register_function(
        "vault::delete",
        RegisterFunction::new_async(move |input: Value| {
            let s = s.clone();
            let i = i.clone();
            async move { vault_delete(s, &i, input).await }
        })
        .description("Remove a credential"),
    );

    let s = state.clone();
    let i = Arc::clone(&bus);
    iii.register_function(
        "vault::rotate",
        RegisterFunction::new_async(move |input: Value| {
            let s = s.clone();
            let i = i.clone();
            async move { vault_rotate(s, &i, input).await }
        })
        .description("Re-encrypt all credentials with a new master password"),
    );

    let s = state.clone();
    let i = Arc::clone(&bus);
    iii.register_function(
        "vault::backup",
        RegisterFunction::new_async(move |input: Value| {
            let s = s.clone();
            let i = i.clone();
            async move { vault_backup(s, &i, input).await }
        })
        .description("Backup current vault state"),
    );

    let s = state.clone();
    let i = Arc::clone(&bus);
    iii.register_function(
        "vault::restore",
        RegisterFunction::new_async(move |input: Value| {
            let s = s.clone();
            let i = i.clone();
            async move { vault_restore(s, &i, input).await }
        })
        .description("Restore vault from backup"),
    );

    for (fn_id, path, method) in [
        ("vault::init", "api/vault/init", "POST"),
        ("vault::set", "api/vault/set", "POST"),
        ("vault::get", "api/vault/get", "POST"),
        ("vault::list", "api/vault/list", "GET"),
        ("vault::delete", "api/vault/delete", "POST"),
        ("vault::rotate", "api/vault/rotate", "POST"),
        ("vault::backup", "api/vault/backup", "POST"),
        ("vault::restore", "api/vault/restore", "POST"),
        // Canonical CLI and TUI routes. The legacy function-shaped routes above
        // remain for direct API consumers.
        ("vault::set", "api/vault/:key", "POST"),
        ("vault::list", "api/vault", "GET"),
        ("vault::delete", "api/vault/:key", "DELETE"),
    ] {
        agentos_http_adapter::register_http_trigger(
            &iii,
            fn_id.to_string(),
            json!({ "api_path": path, "http_method": method }),
            None,
        )?;
    }

    tracing::info!("vault worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static AUTH_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_api_key<T>(value: Option<&str>, test: impl FnOnce() -> T) -> T {
        let _guard = AUTH_ENV_LOCK
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

    #[test]
    fn test_random_bytes_returns_requested_length() {
        let b = random_bytes(32);
        assert_eq!(b.len(), 32);
    }

    #[test]
    fn test_random_bytes_are_random() {
        let a = random_bytes(32);
        let b = random_bytes(32);
        assert_ne!(a, b);
    }

    #[test]
    fn test_derive_key_deterministic() {
        let salt = vec![0u8; 32];
        let k1 = derive_key("password", &salt).unwrap();
        let k2 = derive_key("password", &salt).unwrap();
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 32);
    }

    #[test]
    fn test_derive_key_different_password_different_key() {
        let salt = vec![1u8; 32];
        let k1 = derive_key("password1", &salt).unwrap();
        let k2 = derive_key("password2", &salt).unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_derive_key_different_salt_different_key() {
        let k1 = derive_key("password", &[0u8; 32]).unwrap();
        let k2 = derive_key("password", &[1u8; 32]).unwrap();
        assert_ne!(k1, k2);
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = derive_key("test-password", &[5u8; 32]).unwrap();
        let (iv, ct, tag) = encrypt(&key, "secret-value-123").unwrap();
        let plaintext = decrypt(&key, &iv, &ct, &tag).unwrap();
        assert_eq!(plaintext, "secret-value-123");
    }

    #[test]
    fn test_encrypt_unicode() {
        let key = derive_key("test-password", &[5u8; 32]).unwrap();
        let unicode = "emoji: \u{1f600} CJK: \u{4f60}\u{597d} arabic: \u{0645}\u{0631}\u{062d}\u{0628}\u{0627}";
        let (iv, ct, tag) = encrypt(&key, unicode).unwrap();
        let plaintext = decrypt(&key, &iv, &ct, &tag).unwrap();
        assert_eq!(plaintext, unicode);
    }

    #[test]
    fn test_encrypt_special_chars() {
        let key = derive_key("test-password", &[5u8; 32]).unwrap();
        let special = "key=val&foo=bar\n\ttab \"quotes\" 'single'";
        let (iv, ct, tag) = encrypt(&key, special).unwrap();
        let plaintext = decrypt(&key, &iv, &ct, &tag).unwrap();
        assert_eq!(plaintext, special);
    }

    #[test]
    fn test_encrypt_empty_string() {
        let key = derive_key("test-password", &[5u8; 32]).unwrap();
        let (iv, ct, tag) = encrypt(&key, "").unwrap();
        let plaintext = decrypt(&key, &iv, &ct, &tag).unwrap();
        assert_eq!(plaintext, "");
    }

    #[test]
    fn test_encrypt_long_value() {
        let key = derive_key("test-password", &[5u8; 32]).unwrap();
        let big = "x".repeat(100_000);
        let (iv, ct, tag) = encrypt(&key, &big).unwrap();
        let plaintext = decrypt(&key, &iv, &ct, &tag).unwrap();
        assert_eq!(plaintext, big);
    }

    #[test]
    fn test_encrypt_iv_unique() {
        let key = derive_key("test-password", &[5u8; 32]).unwrap();
        let (iv1, _, _) = encrypt(&key, "same").unwrap();
        let (iv2, _, _) = encrypt(&key, "same").unwrap();
        assert_ne!(iv1, iv2);
    }

    #[test]
    fn test_decrypt_with_wrong_key_fails() {
        let key1 = derive_key("password1", &[5u8; 32]).unwrap();
        let key2 = derive_key("password2", &[5u8; 32]).unwrap();
        let (iv, ct, tag) = encrypt(&key1, "secret").unwrap();
        let result = decrypt(&key2, &iv, &ct, &tag);
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_tampered_ciphertext_fails() {
        let key = derive_key("test-password", &[5u8; 32]).unwrap();
        let (iv, ct, tag) = encrypt(&key, "secret").unwrap();
        let mut tampered = B64.decode(&ct).unwrap();
        if !tampered.is_empty() {
            tampered[0] ^= 0xFF;
        }
        let bad_ct = B64.encode(&tampered);
        let result = decrypt(&key, &iv, &bad_ct, &tag);
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_invalid_iv_length_fails() {
        let key = derive_key("test-password", &[5u8; 32]).unwrap();
        let bad_iv = B64.encode([0u8; 8]);
        let result = decrypt(&key, &bad_iv, "AAAA", "AAAAAAAAAAAAAAAAAAAAAA==");
        assert!(result.is_err());
    }

    #[test]
    fn test_decrypt_invalid_tag_length_fails() {
        let key = derive_key("test-password", &[5u8; 32]).unwrap();
        let iv = B64.encode([0u8; 12]);
        let bad_tag = B64.encode([0u8; 4]);
        let result = decrypt(&key, &iv, "AAAA", &bad_tag);
        assert!(result.is_err());
    }

    #[test]
    fn test_vault_state_default_locked() {
        let st = VaultState::default();
        assert!(!st.unlocked());
    }

    #[test]
    fn test_vault_state_unlocked_when_key_set() {
        let st = VaultState {
            crypto_key: Some(vec![0u8; 32]),
            ..Default::default()
        };
        assert!(st.unlocked());
    }

    #[test]
    fn test_vault_state_lock_clears_key() {
        let mut st = VaultState {
            crypto_key: Some(vec![0u8; 32]),
            last_activity: Some(Instant::now()),
            auto_lock_ms: 1000,
            ..Default::default()
        };
        assert!(st.unlocked());
        st.lock();
        assert!(!st.unlocked());
        assert!(st.last_activity.is_none());
    }

    #[test]
    fn test_vault_state_touch_sets_activity() {
        let mut st = VaultState::default();
        assert!(st.last_activity.is_none());
        st.touch();
        assert!(st.last_activity.is_some());
    }

    #[test]
    fn test_vault_entry_serialization_camel_case() {
        let e = VaultEntry {
            key: "k1".into(),
            iv: "iv".into(),
            ciphertext: "ct".into(),
            tag: "t".into(),
            created_at: 1000,
            updated_at: 2000,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["key"], "k1");
        assert_eq!(v["createdAt"], 1000);
        assert_eq!(v["updatedAt"], 2000);
    }

    #[test]
    fn test_vault_meta_serialization_camel_case() {
        let m = VaultMeta {
            salt: "s".into(),
            created_at: 100,
            rotated_at: Some(200),
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["salt"], "s");
        assert_eq!(v["createdAt"], 100);
        assert_eq!(v["rotatedAt"], 200);
    }

    #[test]
    fn test_vault_meta_skips_none_rotated_at() {
        let m = VaultMeta {
            salt: "s".into(),
            created_at: 100,
            rotated_at: None,
        };
        let v = serde_json::to_value(&m).unwrap();
        assert!(v.get("rotatedAt").is_none());
    }

    #[test]
    fn test_body_or_self_with_body() {
        let req = json!({ "headers": {}, "body": { "key": "value" } });
        let body = body_or_self(&req);
        assert_eq!(body["key"], "value");
    }

    #[test]
    fn test_body_or_self_merges_path_key() {
        let req = json!({ "headers": {}, "body": { "value": "secret" }, "key": "API_KEY" });
        let body = body_or_self(&req);
        assert_eq!(body["key"], "API_KEY");
        assert_eq!(body["value"], "secret");
    }

    #[test]
    fn test_body_or_self_without_body() {
        let req = json!({ "key": "value" });
        let body = body_or_self(&req);
        assert_eq!(body["key"], "value");
    }

    #[test]
    fn test_require_auth_missing_env_fails() {
        let req = json!({});
        assert!(with_api_key(None, || require_auth(&req)).is_err());
    }

    #[test]
    fn test_require_auth_with_correct_token_passes() {
        let req = json!({
            "headers": { "authorization": "Bearer test-key-passes" }
        });
        let result = with_api_key(Some("test-key-passes"), || require_auth(&req));
        assert!(result.is_ok());
    }

    #[test]
    fn test_require_auth_with_wrong_token_fails() {
        let req = json!({
            "headers": { "authorization": "Bearer wrong-token" }
        });
        let result = with_api_key(Some("expected-wrong"), || require_auth(&req));
        assert!(result.is_err());
    }

    #[test]
    fn test_require_auth_missing_header_fails() {
        let req = json!({});
        let result = with_api_key(Some("expected-mh"), || require_auth(&req));
        assert!(result.is_err());
    }

    #[test]
    fn test_require_auth_empty_token_fails() {
        let req = json!({
            "headers": { "authorization": "Bearer " }
        });
        let result = with_api_key(Some("expected-et"), || require_auth(&req));
        assert!(result.is_err());
    }

    fn unlocked_state() -> SharedState {
        Arc::new(tokio::sync::Mutex::new(VaultState {
            crypto_key: Some(vec![7u8; 32]),
            last_activity: Some(Instant::now()),
            ..Default::default()
        }))
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(future)
    }

    /// A client that was never connected: `IIIClient::new` does not open a
    /// socket, so any handler that reaches `iii.trigger` would block on the
    /// SDK timeout. Every assertion below must therefore return before the
    /// first state call, which is exactly what an auth gate does.
    fn offline_client() -> Bus {
        Arc::new(iii_sdk::IIIClient::new("ws://127.0.0.1:1"))
    }

    #[test]
    fn vault_get_rejects_bus_caller_without_headers() {
        let request = json!({ "key": "ANTHROPIC_API_KEY" });
        let error = with_api_key(Some("bus-caller-get"), || {
            block_on(vault_get(unlocked_state(), &offline_client(), request))
        })
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("Unauthorized"),
            "bus caller must be rejected before the vault is read, got: {error}"
        );
    }

    #[test]
    fn vault_get_rejects_wrong_bearer() {
        let request = json!({
            "headers": { "authorization": "Bearer not-the-key" },
            "body": { "key": "ANTHROPIC_API_KEY" },
        });
        let error = with_api_key(Some("vault-get-expected"), || {
            block_on(vault_get(unlocked_state(), &offline_client(), request))
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("Unauthorized"), "got: {error}");
    }

    #[test]
    fn vault_list_rejects_bus_caller_without_headers() {
        let error = with_api_key(Some("vault-list-expected"), || {
            block_on(vault_list(unlocked_state(), &offline_client(), json!({})))
        })
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("Unauthorized"),
            "bus caller must be rejected before the key list is read, got: {error}"
        );
    }

    #[test]
    fn vault_list_rejects_wrong_bearer() {
        let request = json!({ "headers": { "authorization": "Bearer nope" } });
        let error = with_api_key(Some("vault-list-expected-2"), || {
            block_on(vault_list(unlocked_state(), &offline_client(), request))
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("Unauthorized"), "got: {error}");
    }

    // --- tenancy (contract T1) through the real handlers, on a FakeBus ---

    use agentos_http_adapter::fake::FakeBus;
    use agentos_http_adapter::policy;
    use std::collections::BTreeMap;

    /// In-memory `state::*` with the engine's real shapes: `state::get` of a
    /// missing key is null, `state::list` is a bare array of stored values.
    #[derive(Default)]
    struct StateStore(Mutex<BTreeMap<String, BTreeMap<String, Value>>>);

    impl StateStore {
        fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, BTreeMap<String, Value>>> {
            self.0.lock().unwrap_or_else(|error| error.into_inner())
        }
        fn field(input: &Value, name: &str) -> String {
            input[name].as_str().unwrap_or_default().to_string()
        }
    }

    /// A bus with a state store, plus a capability reader whose store grants
    /// `a-get` exactly `vault::get`, and `a-wild` every wildcard a capability
    /// document can express. The reader answers through the shared matcher,
    /// so what the tests prove is the real I1 rule.
    fn state_bus() -> (Arc<FakeBus>, Arc<StateStore>) {
        let store = Arc::new(StateStore::default());
        let bus = Arc::new(FakeBus::new());
        let state = store.clone();
        bus.on("state::get", move |input| {
            Ok(state
                .lock()
                .get(&StateStore::field(&input, "scope"))
                .and_then(|scope| scope.get(&StateStore::field(&input, "key")))
                .cloned()
                .unwrap_or(Value::Null))
        });
        let state = store.clone();
        bus.on("state::set", move |input| {
            state
                .lock()
                .entry(StateStore::field(&input, "scope"))
                .or_default()
                .insert(StateStore::field(&input, "key"), input["value"].clone());
            Ok(json!({ "stored": true }))
        });
        let state = store.clone();
        bus.on("state::delete", move |input| {
            if let Some(scope) = state.lock().get_mut(&StateStore::field(&input, "scope")) {
                scope.remove(&StateStore::field(&input, "key"));
            }
            Ok(json!({ "deleted": true }))
        });
        let state = store.clone();
        bus.on("state::list", move |input| {
            Ok(Value::Array(
                state
                    .lock()
                    .get(&StateStore::field(&input, "scope"))
                    .map(|scope| scope.values().cloned().collect())
                    .unwrap_or_default(),
            ))
        });
        bus.on_value("security::audit", json!({ "ok": true }));
        bus.on("security::check_capability", |input| {
            let agent = input["agentId"].as_str().unwrap_or_default();
            let resource = input["resource"].as_str().unwrap_or_default();
            let tools: Vec<String> = match agent {
                "a-get" => vec!["vault::get".into()],
                "a-wild" => vec!["*".into(), "vault::*".into(), "grant::*".into()],
                _ => vec![],
            };
            if policy::capabilities_grant(&tools, resource) {
                Ok(json!({ "allowed": true, "reason": "granted" }))
            } else {
                Err(Error::Handler(format!("Agent {agent} denied: {resource}")))
            }
        });
        (bus, store)
    }

    fn as_bus(bus: &Arc<FakeBus>) -> Bus {
        Arc::clone(bus) as Bus
    }

    /// What the HTTP adapter forwards for an authenticated edge request.
    fn as_operator(token: &str, body: Value) -> Value {
        json!({
            "headers": { "authorization": format!("Bearer {token}") },
            "body": body,
        })
    }

    fn as_agent(agent: &str, fields: Value) -> Value {
        let mut payload = fields;
        payload["principal"] = principal::as_agent(agent);
        payload
    }

    fn fresh_state() -> SharedState {
        Arc::new(tokio::sync::Mutex::new(VaultState {
            auto_lock_ms: DEFAULT_AUTO_LOCK_MS,
            ..Default::default()
        }))
    }

    fn state_calls(bus: &FakeBus) -> usize {
        bus.calls()
            .iter()
            .filter(|call| call.function_id.starts_with("state::"))
            .count()
    }

    #[test]
    fn the_operator_round_trips_a_credential_through_the_real_handlers() {
        with_api_key(Some("op-key"), || {
            block_on(async {
                let (bus, _store) = state_bus();
                let iii = as_bus(&bus);
                let state = fresh_state();

                let unlocked = vault_init(
                    state.clone(),
                    &iii,
                    as_operator("op-key", json!({ "password": "correct horse battery" })),
                )
                .await
                .expect("init");
                assert_eq!(unlocked["unlocked"], true);

                vault_set(
                    state.clone(),
                    &iii,
                    as_operator("op-key", json!({ "key": "API_KEY", "value": "s3cret" })),
                )
                .await
                .expect("set");
                let got = vault_get(
                    state.clone(),
                    &iii,
                    as_operator("op-key", json!({ "key": "API_KEY" })),
                )
                .await
                .expect("get");
                assert_eq!(got["value"], "s3cret");

                let listed = vault_list(state.clone(), &iii, as_operator("op-key", json!({})))
                    .await
                    .expect("list");
                assert_eq!(
                    listed["count"], 1,
                    "list must read the bare values the engine returns"
                );
                assert_eq!(listed["keys"][0]["key"], "API_KEY");

                vault_delete(
                    state.clone(),
                    &iii,
                    as_operator("op-key", json!({ "key": "API_KEY" })),
                )
                .await
                .expect("delete");
                assert!(
                    vault_get(
                        state,
                        &iii,
                        as_operator("op-key", json!({ "key": "API_KEY" }))
                    )
                    .await
                    .is_err()
                );
                assert_eq!(
                    bus.call_count("security::check_capability"),
                    0,
                    "the operator needs no grant"
                );
            })
        });
    }

    #[test]
    fn an_agent_id_in_a_vault_payload_is_refused_before_anything_is_read() {
        with_api_key(Some("op-key"), || {
            block_on(async {
                let (bus, _store) = state_bus();
                let iii = as_bus(&bus);
                let state = unlocked_state();

                // In the body (HTTP) and at the top level (bus): both spellings.
                for payload in [
                    as_operator("op-key", json!({ "key": "K", "agentId": "a-1" })),
                    as_operator("op-key", json!({ "key": "K", "agent": "a-1" })),
                    {
                        let mut top = as_operator("op-key", json!({ "key": "K" }));
                        top["agentId"] = json!("a-1");
                        top
                    },
                    // An agent principal naming somebody else.
                    as_agent("a-get", json!({ "key": "K", "agentId": "a-2" })),
                ] {
                    let error = vault_get(state.clone(), &iii, payload.clone())
                        .await
                        .unwrap_err()
                        .to_string();
                    assert!(
                        error.contains("no per-agent namespace"),
                        "{payload}: {error}"
                    );
                    assert!(
                        vault_set(state.clone(), &iii, payload.clone())
                            .await
                            .is_err()
                    );
                    assert!(
                        vault_list(state.clone(), &iii, payload.clone())
                            .await
                            .is_err()
                    );
                    assert!(vault_delete(state.clone(), &iii, payload).await.is_err());
                }
                assert_eq!(state_calls(&bus), 0, "refused before the store is touched");
            })
        });
    }

    #[test]
    fn an_agent_principal_needs_the_exact_vault_capability() {
        with_api_key(Some("op-key"), || {
            block_on(async {
                let (bus, _store) = state_bus();
                let iii = as_bus(&bus);
                let state = fresh_state();
                vault_init(
                    state.clone(),
                    &iii,
                    as_operator("op-key", json!({ "password": "correct horse battery" })),
                )
                .await
                .expect("init");
                vault_set(
                    state.clone(),
                    &iii,
                    as_operator("op-key", json!({ "key": "API_KEY", "value": "s3cret" })),
                )
                .await
                .expect("set");
                let before = state_calls(&bus);

                // Exact `vault::get`: allowed, and only for get.
                let got = vault_get(
                    state.clone(),
                    &iii,
                    as_agent("a-get", json!({ "key": "API_KEY" })),
                )
                .await
                .expect("exact grant");
                assert_eq!(got["value"], "s3cret");
                let same_agent = as_agent("a-get", json!({ "key": "API_KEY", "agentId": "a-get" }));
                assert!(vault_get(state.clone(), &iii, same_agent).await.is_ok());
                let denied = vault_set(
                    state.clone(),
                    &iii,
                    as_agent("a-get", json!({ "key": "API_KEY", "value": "x" })),
                )
                .await
                .unwrap_err()
                .to_string();
                assert!(denied.contains("not granted vault::set"), "{denied}");

                // Every wildcard a capability document can express: nothing.
                for (agent, why) in [
                    ("a-wild", "wildcards never reach vault::*"),
                    ("a-none", "no record"),
                ] {
                    let error = vault_get(
                        state.clone(),
                        &iii,
                        as_agent(agent, json!({ "key": "API_KEY" })),
                    )
                    .await
                    .unwrap_err()
                    .to_string();
                    assert!(error.contains("not granted vault::get"), "{why}: {error}");
                }
                let asked = bus.calls_to("security::check_capability");
                assert!(asked.iter().all(|call| {
                    call.payload["resource"]
                        .as_str()
                        .is_some_and(|resource| resource.starts_with("vault::"))
                }));
                assert_eq!(
                    state_calls(&bus),
                    before + 2,
                    "only the two granted reads touched the store"
                );
            })
        });
    }

    #[test]
    fn key_management_is_operator_only_whatever_an_agent_is_granted() {
        with_api_key(Some("op-key"), || {
            block_on(async {
                let (bus, _store) = state_bus();
                let iii = as_bus(&bus);
                let state = unlocked_state();
                let agent = as_agent(
                    "a-wild",
                    json!({
                        "password": "correct horse battery",
                        "currentPassword": "correct horse battery",
                        "newPassword": "another long password",
                    }),
                );
                for result in [
                    vault_init(state.clone(), &iii, agent.clone()).await,
                    vault_rotate(state.clone(), &iii, agent.clone()).await,
                    vault_backup(state.clone(), &iii, agent.clone()).await,
                    vault_restore(state.clone(), &iii, agent.clone()).await,
                ] {
                    let error = result.unwrap_err().to_string();
                    assert!(error.contains("Unauthorized"), "{error}");
                }
                assert!(
                    bus.calls().is_empty(),
                    "refused before any bus call, got {:?}",
                    bus.calls()
                );

                // And a bare bus payload — no bearer, no principal — is refused
                // from the credential operations for the same reason.
                let bare = json!({ "key": "API_KEY", "value": "x" });
                assert!(vault_set(state.clone(), &iii, bare.clone()).await.is_err());
                assert!(vault_delete(state, &iii, bare).await.is_err());
                assert!(bus.calls().is_empty());
            })
        });
    }

    #[test]
    fn rotation_re_encrypts_the_credentials_the_engine_actually_returns() {
        with_api_key(Some("op-key"), || {
            block_on(async {
                let (bus, store) = state_bus();
                let iii = as_bus(&bus);
                let state = fresh_state();
                vault_init(
                    state.clone(),
                    &iii,
                    as_operator("op-key", json!({ "password": "correct horse battery" })),
                )
                .await
                .expect("init");
                for (key, value) in [("ONE", "first"), ("TWO", "second")] {
                    vault_set(
                        state.clone(),
                        &iii,
                        as_operator("op-key", json!({ "key": key, "value": value })),
                    )
                    .await
                    .expect("set");
                }

                let rotated = vault_rotate(
                    state.clone(),
                    &iii,
                    as_operator(
                        "op-key",
                        json!({
                            "currentPassword": "correct horse battery",
                            "newPassword": "another long password",
                        }),
                    ),
                )
                .await
                .expect("rotate");
                assert_eq!(
                    rotated["rotated"], 2,
                    "the envelope reader this worker used to have rotated zero credentials"
                );

                // Readable under the new key, and the backup holds the old copies.
                let got = vault_get(
                    state.clone(),
                    &iii,
                    as_operator("op-key", json!({ "key": "TWO" })),
                )
                .await
                .expect("get after rotate");
                assert_eq!(got["value"], "second");
                assert_eq!(
                    store.lock().get("vault_backup").map(BTreeMap::len),
                    Some(3),
                    "__meta plus both credentials"
                );

                let backed_up = vault_backup(state.clone(), &iii, as_operator("op-key", json!({})))
                    .await
                    .expect("backup");
                assert_eq!(backed_up["backedUp"], 2);
                let restored = vault_restore(state, &iii, as_operator("op-key", json!({})))
                    .await
                    .expect("restore");
                assert_eq!(restored["restored"], 2);
            })
        });
    }

    // --- F5: rotation is atomic against the store and against other writers ---

    /// A `state::set` failure injected by scope, key and occurrence: the Nth
    /// write of that key after installation (counting from 1) fails.
    fn failing_state_set(bus: &FakeBus, store: &Arc<StateStore>, failures: &[(&str, &str, usize)]) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let failures: Vec<(String, String, usize)> = failures
            .iter()
            .map(|(scope, key, nth)| (scope.to_string(), key.to_string(), *nth))
            .collect();
        let counts: Arc<Mutex<BTreeMap<(String, String), usize>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let armed = Arc::new(AtomicUsize::new(1));
        let state = Arc::clone(store);
        bus.on("state::set", move |input| {
            let scope = StateStore::field(&input, "scope");
            let key = StateStore::field(&input, "key");
            let nth = {
                let mut counts = counts.lock().unwrap_or_else(|error| error.into_inner());
                let count = counts.entry((scope.clone(), key.clone())).or_insert(0);
                *count += 1;
                *count
            };
            if armed.load(Ordering::SeqCst) == 1
                && failures
                    .iter()
                    .any(|(s, k, n)| *s == scope && *k == key && *n == nth)
            {
                return Err(Error::Handler(format!(
                    "injected: state store refused write #{nth} of {scope}/{key}"
                )));
            }
            state
                .lock()
                .entry(scope)
                .or_default()
                .insert(key, input["value"].clone());
            Ok(json!({ "stored": true }))
        });
    }

    async fn settle_spawned_audits() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    fn rotate_input(current: &str, new: &str) -> Value {
        as_operator(
            "op-key",
            json!({ "currentPassword": current, "newPassword": new }),
        )
    }

    async fn unlocked_with(state: &SharedState, iii: &Bus, entries: &[(&str, &str)]) {
        vault_init(
            state.clone(),
            iii,
            as_operator("op-key", json!({ "password": "correct horse battery" })),
        )
        .await
        .expect("init");
        for (key, value) in entries {
            vault_set(
                state.clone(),
                iii,
                as_operator("op-key", json!({ "key": key, "value": value })),
            )
            .await
            .expect("set");
        }
    }

    async fn read(state: &SharedState, iii: &Bus, key: &str) -> Result<Value, Error> {
        vault_get(
            state.clone(),
            iii,
            as_operator("op-key", json!({ "key": key })),
        )
        .await
    }

    #[test]
    fn a_rotation_that_fails_mid_loop_rolls_back_and_the_store_stays_readable() {
        with_api_key(Some("op-key"), || {
            block_on(async {
                let (bus, store) = state_bus();
                let iii = as_bus(&bus);
                let state = fresh_state();
                unlocked_with(&state, &iii, &[("ONE", "first"), ("TWO", "second")]).await;
                let before = store.lock().get("vault").cloned().expect("vault scope");
                let old_salt = before["__meta"]["salt"].clone();

                // ONE's re-key write succeeds; TWO's fails.
                failing_state_set(&bus, &store, &[("vault", "TWO", 1)]);

                let error = vault_rotate(
                    state.clone(),
                    &iii,
                    rotate_input("correct horse battery", "another long password"),
                )
                .await
                .unwrap_err()
                .to_string();
                assert!(error.contains("rolled back"), "{error}");
                assert!(error.contains("injected"), "the cause is reported: {error}");

                // Every entry is byte-for-byte what it was, under the old salt.
                let after = store.lock().get("vault").cloned().expect("vault scope");
                assert_eq!(
                    after["ONE"], before["ONE"],
                    "ONE was re-keyed and must be restored"
                );
                assert_eq!(after["TWO"], before["TWO"]);
                assert_eq!(after["__meta"]["salt"], old_salt);

                // And readable with the key still in memory: nothing switched.
                assert_eq!(
                    read(&state, &iii, "ONE").await.expect("ONE")["value"],
                    "first"
                );
                assert_eq!(
                    read(&state, &iii, "TWO").await.expect("TWO")["value"],
                    "second"
                );

                settle_spawned_audits().await;
                let audit = bus
                    .calls_to("security::audit")
                    .into_iter()
                    .map(|call| call.payload)
                    .find(|payload| payload["type"] == "vault_rotation_failed")
                    .expect("the failure is audited");
                assert_eq!(audit["detail"]["rolledBack"], true);
                assert_eq!(audit["detail"]["unrestored"], json!([]));

                // A retry with the same passwords now succeeds: the store was
                // left consistent, not half-rotated.
                let rotated = vault_rotate(
                    state.clone(),
                    &iii,
                    rotate_input("correct horse battery", "another long password"),
                )
                .await
                .expect("retry");
                assert_eq!(rotated["rotated"], 2);
                assert_eq!(
                    read(&state, &iii, "TWO").await.expect("TWO")["value"],
                    "second"
                );
            })
        });
    }

    #[test]
    fn a_rollback_that_fails_is_reported_not_swallowed() {
        with_api_key(Some("op-key"), || {
            block_on(async {
                let (bus, store) = state_bus();
                let iii = as_bus(&bus);
                let state = fresh_state();
                unlocked_with(&state, &iii, &[("ONE", "first"), ("TWO", "second")]).await;

                // TWO's re-key write fails, and then the rollback of ONE (its
                // second write since the fault was installed: re-key, restore)
                // fails as well.
                failing_state_set(&bus, &store, &[("vault", "TWO", 1), ("vault", "ONE", 2)]);

                let error = vault_rotate(
                    state.clone(),
                    &iii,
                    rotate_input("correct horse battery", "another long password"),
                )
                .await
                .unwrap_err()
                .to_string();
                assert!(
                    !error.contains("rolled back:"),
                    "a rollback that failed must not be reported as done: {error}"
                );
                assert!(error.contains("rollback incomplete"), "{error}");
                assert!(
                    error.contains("ONE"),
                    "the unrestored key is named: {error}"
                );
                assert!(
                    error.contains("vault::restore"),
                    "the recovery is named: {error}"
                );

                settle_spawned_audits().await;
                let audit = bus
                    .calls_to("security::audit")
                    .into_iter()
                    .map(|call| call.payload)
                    .find(|payload| payload["type"] == "vault_rotation_failed")
                    .expect("the failure is audited");
                assert_eq!(audit["detail"]["rolledBack"], false);
                assert_eq!(audit["detail"]["unrestored"], json!(["ONE"]));

                // The in-memory key did not switch, so TWO (never re-keyed) still
                // reads, and the backup holds ONE for `vault::restore`.
                assert_eq!(
                    read(&state, &iii, "TWO").await.expect("TWO")["value"],
                    "second"
                );
                let restored = vault_restore(
                    state.clone(),
                    &iii,
                    as_operator("op-key", json!({ "password": "correct horse battery" })),
                )
                .await
                .expect("restore from the backup the rotation wrote first");
                assert_eq!(restored["restored"], 2);
                assert_eq!(
                    read(&state, &iii, "ONE").await.expect("ONE")["value"],
                    "first"
                );
            })
        });
    }

    /// A bus that parks the first credential write of a rotation until the
    /// test releases it, so another handler can be driven in the gap.
    struct GatedBus {
        inner: Arc<FakeBus>,
        armed: std::sync::atomic::AtomicBool,
        reached: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    impl TriggerBus for GatedBus {
        fn trigger(&self, request: TriggerRequest) -> agentos_http_adapter::bus::BusFuture<'_> {
            Box::pin(async move {
                let is_credential_write = request.function_id == "state::set"
                    && request.payload["scope"] == "vault"
                    && request.payload["key"] != "__meta";
                if is_credential_write
                    && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    self.reached.notify_one();
                    self.release.notified().await;
                }
                self.inner.trigger(request).await
            })
        }
    }

    #[test]
    fn no_credential_write_interleaves_with_a_rotation() {
        with_api_key(Some("op-key"), || {
            block_on(async {
                let (fake, _store) = state_bus();
                let gated = Arc::new(GatedBus {
                    inner: Arc::clone(&fake),
                    armed: std::sync::atomic::AtomicBool::new(false),
                    reached: tokio::sync::Notify::new(),
                    release: tokio::sync::Notify::new(),
                });
                let iii: Bus = Arc::clone(&gated) as Bus;
                let state = fresh_state();
                unlocked_with(&state, &iii, &[("ONE", "first")]).await;

                // Park the rotation at its first re-key write, after it has
                // listed the credentials and before it switches the key.
                gated.armed.store(true, std::sync::atomic::Ordering::SeqCst);
                let rotation = tokio::spawn({
                    let state = state.clone();
                    let iii = Arc::clone(&iii);
                    async move {
                        vault_rotate(
                            state,
                            &iii,
                            rotate_input("correct horse battery", "another long password"),
                        )
                        .await
                    }
                });
                gated.reached.notified().await;

                // A credential write arriving now must wait for the rotation to
                // finish; if it ran, it would encrypt with the OLD key and be
                // orphaned the moment `__meta` switches.
                let write = tokio::spawn({
                    let state = state.clone();
                    let iii = Arc::clone(&iii);
                    async move {
                        vault_set(
                            state,
                            &iii,
                            as_operator("op-key", json!({ "key": "THREE", "value": "third" })),
                        )
                        .await
                    }
                });
                for _ in 0..4 {
                    tokio::task::yield_now().await;
                }
                assert!(
                    fake.calls_to("state::set")
                        .iter()
                        .all(|call| call.payload["key"] != "THREE"),
                    "a credential write ran in the middle of a rotation"
                );

                gated.release.notify_one();
                let rotated = rotation.await.expect("join").expect("rotate");
                assert_eq!(rotated["rotated"], 1);
                write
                    .await
                    .expect("join")
                    .expect("the parked write completes after the rotation");

                // Written after the switch, so under the key now in memory.
                assert_eq!(
                    read(&state, &iii, "THREE").await.expect("THREE")["value"],
                    "third"
                );
                assert_eq!(
                    read(&state, &iii, "ONE").await.expect("ONE")["value"],
                    "first"
                );
            })
        });
    }

    #[test]
    fn stored_credentials_read_the_bare_shape_and_tolerate_the_envelope() {
        let bare = json!({ "key": "K", "iv": "i", "ciphertext": "c", "tag": "t" });
        let meta = json!({ "key": "__meta", "salt": "s" });
        let enveloped = json!({ "key": "E", "value": { "key": "E", "iv": "i", "ciphertext": "c", "tag": "t" } });
        let empty = json!({ "key": "X", "ciphertext": "" });
        let credentials = stored_credentials(vec![bare, meta, enveloped, empty, Value::Null]);
        let keys: Vec<&str> = credentials.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, vec!["K", "E"]);
        assert_eq!(credentials[1].1["ciphertext"], "c");
    }

    #[test]
    fn test_now_ms_positive() {
        assert!(now_ms() > 0);
    }

    #[test]
    fn test_constants() {
        assert_eq!(SALT_LEN, 32);
        assert_eq!(NONCE_LEN, 12);
        assert_eq!(TAG_LEN, 16);
        assert_eq!(MIN_PASSWORD_LEN, 8);
        assert_eq!(DEFAULT_AUTO_LOCK_MS, 30 * 60 * 1000);
    }

    #[test]
    fn test_encrypt_outputs_are_base64() {
        let key = derive_key("test-password", &[5u8; 32]).unwrap();
        let (iv, ct, tag) = encrypt(&key, "hello").unwrap();
        assert!(B64.decode(&iv).is_ok());
        assert!(B64.decode(&ct).is_ok());
        assert!(B64.decode(&tag).is_ok());
    }

    #[test]
    fn test_encrypt_iv_decodes_to_12_bytes() {
        let key = derive_key("test-password", &[5u8; 32]).unwrap();
        let (iv, _, _) = encrypt(&key, "hello").unwrap();
        let decoded = B64.decode(&iv).unwrap();
        assert_eq!(decoded.len(), NONCE_LEN);
    }

    #[test]
    fn test_encrypt_tag_decodes_to_16_bytes() {
        let key = derive_key("test-password", &[5u8; 32]).unwrap();
        let (_, _, tag) = encrypt(&key, "hello").unwrap();
        let decoded = B64.decode(&tag).unwrap();
        assert_eq!(decoded.len(), TAG_LEN);
    }
}
