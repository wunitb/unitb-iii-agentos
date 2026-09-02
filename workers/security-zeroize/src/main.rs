use dashmap::DashMap;
use iii_sdk::errors::Error;
use iii_sdk::{InitOptions, RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use std::sync::OnceLock;
use std::time::Duration;
use zeroize::Zeroizing;

const AUTO_DISPOSE_MS: u64 = 30_000;

fn registry() -> &'static DashMap<String, Zeroizing<Vec<u8>>> {
    static REG: OnceLock<DashMap<String, Zeroizing<Vec<u8>>>> = OnceLock::new();
    REG.get_or_init(DashMap::new)
}

fn secret_patterns() -> &'static Vec<regex::Regex> {
    static PATTERNS: OnceLock<Vec<regex::Regex>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        SECRET_PATTERNS
            .iter()
            .filter_map(|p| regex::Regex::new(p).ok())
            .collect()
    })
}

static SECRET_PATTERNS: &[&str] = &[
    r"(?i)(?:api[_-]?key|apikey)\s*[:=]\s*\S+",
    r"(?i)(?:secret|password|passwd|token)\s*[:=]\s*\S+",
    r"(?:sk|pk)[-_][a-zA-Z0-9]{20,}",
    r"ghp_[a-zA-Z0-9]{36}",
    r"xox[bpas]-[a-zA-Z0-9\-]+",
    r"-----BEGIN (?:RSA |EC )?PRIVATE KEY-----",
    r"Bearer\s+[a-zA-Z0-9._\-]{20,}",
];

fn wrap_secret(value: &str, auto_dispose_ms: u64) -> String {
    let id = uuid_v4();
    let bytes = Zeroizing::new(value.as_bytes().to_vec());
    registry().insert(id.clone(), bytes);

    let id_clone = id.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(auto_dispose_ms)).await;
        registry().remove(&id_clone);
    });

    id
}

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("zb-{:x}", nanos)
}

/// A stable label for one entry of a `state::list` result.
///
/// `state::list` answers a bare array of stored values, so the storage key is
/// simply not available. The document's own `id` is the closest identifier a
/// finding can carry; otherwise the position in the scope is reported so an
/// operator can still locate the entry.
fn entry_label(entry: &Value, index: usize) -> String {
    entry
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|id| !id.is_empty())
        .map(String::from)
        .unwrap_or_else(|| format!("#{index}"))
}

/// Scan one `state::list` response for secrets.
///
/// The engine answers a bare array of the stored values themselves: there is
/// no `{key, value}` envelope. Reading `entry["value"]` therefore scanned an
/// empty string for every entry, which is why this scan never reported a
/// finding.
fn scan_scope_entries(scope: &str, entries: &Value) -> Vec<Value> {
    let Some(items) = entries.as_array() else {
        return Vec::new();
    };
    let mut findings = Vec::new();
    for (index, entry) in items.iter().enumerate() {
        if entry.is_null() {
            continue;
        }
        let matched = scan_value_for_secrets(entry);
        if matched.is_empty() {
            continue;
        }
        findings.push(json!({
            "scope": scope,
            "key": entry_label(entry, index),
            "patterns": matched,
        }));
    }
    findings
}

fn scan_value_for_secrets(value: &Value) -> Vec<String> {
    let s = serde_json::to_string(value).unwrap_or_default();
    secret_patterns()
        .iter()
        .filter(|re| re.is_match(&s))
        .map(|re| {
            let src = re.as_str();
            let take = src.len().min(30);
            src[..take].to_string()
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, InitOptions::default());

    iii.register_function(
        "security::zeroize_wrap",
        RegisterFunction::new_async(move |input: Value| async move {
            let value = input
                .get("value")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::Handler("value is required".into()))?;
            if value.is_empty() {
                return Err(Error::Handler("value is required".into()));
            }
            let auto_dispose_ms = input
                .get("autoDisposeMs")
                .and_then(|v| v.as_u64())
                .unwrap_or(AUTO_DISPOSE_MS);

            let id = wrap_secret(value, auto_dispose_ms);
            Ok::<Value, Error>(json!({
                "wrapped": true,
                "id": id,
                "autoDisposeMs": auto_dispose_ms,
            }))
        })
        .description("Wrap a secret string in a zeroized buffer"),
    );

    let iii_ref = iii.clone();
    iii.register_function(
        "security::zeroize_check",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_ref.clone();
            async move {
                let target_scopes: Vec<String> = input
                    .get("scopes")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_else(|| vec!["config".into(), "sessions".into(), "agents".into()]);

                let mut findings: Vec<Value> = Vec::new();

                for scope in &target_scopes {
                    let entries = iii
                        .trigger(TriggerRequest {
                            function_id: "state::list".to_string(),
                            payload: json!({ "scope": scope }),
                            action: None,
                            timeout_ms: None,
                        })
                        .await
                        .unwrap_or(json!([]));

                    findings.extend(scan_scope_entries(scope, &entries));
                }

                if !findings.is_empty() {
                    let _iii = iii.clone();
                    let count = findings.len();
                    let scopes_clone = target_scopes.clone();
                    tokio::spawn(async move {
                        let _ = _iii
                            .trigger(TriggerRequest {
                                function_id: "security::audit".to_string(),
                                payload: json!({
                                    "type": "zeroize_scan_findings",
                                    "detail": { "count": count, "scopes": scopes_clone },
                                }),
                                action: None,
                                timeout_ms: None,
                            })
                            .await;
                    });
                }

                Ok::<Value, Error>(json!({
                    "clean": findings.is_empty(),
                    "findings": findings,
                    "scanned": target_scopes.len(),
                }))
            }
        })
        .description("Scan KV state for potential unzeroized secrets"),
    );

    tracing::info!("security-zeroize worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_secret_patterns_compile() {
        assert_eq!(secret_patterns().len(), SECRET_PATTERNS.len());
    }

    #[test]
    fn test_scan_detects_api_key() {
        let v = json!("api_key=abcdef1234567890");
        let matches = scan_value_for_secrets(&v);
        assert!(!matches.is_empty());
    }

    #[test]
    fn test_scan_detects_password() {
        let v = json!("password=hunter2hello");
        let matches = scan_value_for_secrets(&v);
        assert!(!matches.is_empty());
    }

    #[test]
    fn test_scan_detects_github_token() {
        let v = json!(format!("{}{}", "ghp_", "a".repeat(36)));
        let matches = scan_value_for_secrets(&v);
        assert!(!matches.is_empty());
    }

    #[test]
    fn test_scan_detects_slack_token() {
        let v = json!(format!("{}{}", "xoxb-", "12345-abcdef"));
        let matches = scan_value_for_secrets(&v);
        assert!(!matches.is_empty());
    }

    #[test]
    fn test_scan_detects_pem_private_key() {
        let v = json!(format!("-----BEGIN {} PRIVATE KEY-----", "RSA"));
        let matches = scan_value_for_secrets(&v);
        assert!(!matches.is_empty());
    }

    #[test]
    fn test_scan_detects_bearer_token() {
        let v = json!("Authorization: Bearer abcdef1234567890abcdef1234");
        let matches = scan_value_for_secrets(&v);
        assert!(!matches.is_empty());
    }

    #[test]
    fn test_scan_clean_value() {
        let v = json!({ "name": "alice", "age": 30 });
        let matches = scan_value_for_secrets(&v);
        assert!(matches.is_empty());
    }

    #[test]
    fn test_scan_empty_object() {
        let v = json!({});
        let matches = scan_value_for_secrets(&v);
        assert!(matches.is_empty());
    }

    #[test]
    fn test_pattern_truncation_under_30() {
        let v = json!(format!("{}{}", "ghp_", "a".repeat(36)));
        let matches = scan_value_for_secrets(&v);
        for m in &matches {
            assert!(m.len() <= 30);
        }
    }

    #[tokio::test]
    async fn test_wrap_secret_inserts_in_registry() {
        let id = wrap_secret("super-secret", 60_000);
        assert!(registry().contains_key(&id));
        registry().remove(&id);
    }

    #[test]
    fn test_zeroizing_buffer_holds_data() {
        let buf = Zeroizing::new(b"hello".to_vec());
        assert_eq!(&*buf, b"hello");
    }

    // --- state::list protocol (verified against iii 0.22.1) ---

    #[test]
    fn scan_reads_the_bare_values_state_list_returns() {
        // `iii trigger state::list scope=config` answers the stored values
        // themselves, with no key and no `{key, value}` envelope.
        let entries = json!([
            { "id": "prod", "note": "api_key=abcdef1234567890" },
            { "id": "dev", "note": "nothing to see" }
        ]);
        let findings = scan_scope_entries("config", &entries);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0]["scope"], "config");
        assert_eq!(findings[0]["key"], "prod");
        assert!(!findings[0]["patterns"].as_array().unwrap().is_empty());
    }

    #[test]
    fn the_envelope_this_worker_used_to_expect_hid_every_secret() {
        // The old reader scanned `entry["value"]`, defaulting to "" when the
        // field was absent - which it always is - so the scan could never
        // report a finding. Reproduce that read over a real list response.
        let entries = json!([{ "id": "prod", "note": "api_key=abcdef1234567890" }]);
        let old_read = entries[0].get("value").cloned().unwrap_or(json!(""));
        assert!(scan_value_for_secrets(&old_read).is_empty());
        assert!(!scan_scope_entries("config", &entries).is_empty());
    }

    #[test]
    fn an_entry_without_an_id_is_reported_by_position() {
        let entries = json!([
            { "note": "clean" },
            { "note": "password=hunter2hello" }
        ]);
        let findings = scan_scope_entries("sessions", &entries);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0]["key"], "#1");
    }

    #[test]
    fn deleted_entries_and_non_array_responses_are_ignored() {
        // `state::set value=null` leaves a null entry in the scope.
        assert!(scan_scope_entries("config", &json!([null])).is_empty());
        assert!(scan_scope_entries("config", &json!(null)).is_empty());
        assert!(scan_scope_entries("config", &json!({ "entries": [] })).is_empty());
    }
}
