use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, InitOptions, RegisterFunction, protocol::TriggerRequest, register_worker,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::process::Command;

mod types;

use types::{BrowserSession, Viewport};

const MAX_SESSIONS: usize = 5;
const IDLE_TIMEOUT_MS: i64 = 5 * 60 * 1000;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;

const BRIDGE_SCRIPT: &str = r#"
import ipaddress, json, socket, sys
from urllib.parse import urlparse
from playwright.sync_api import sync_playwright

# In-page SSRF guard.
#
# The Rust side validates the URL it is asked to open, but `page.goto` follows
# redirects and a page issues sub-resource requests, so a public URL can still
# reach 169.254.169.254 or 127.0.0.1 one hop later. Every request the browser
# makes is therefore re-checked here, at request time, and aborted before it
# leaves the machine. Anything we cannot positively prove is public is aborted.

_RESOLVED = {}
_BLOCKED_NAMES = {
    "localhost",
    "metadata",
    "metadata.google.internal",
    "ip6-localhost",
    "ip6-loopback",
}
_BLOCKED_SUFFIXES = (".localhost", ".internal", ".local")


def _blocked_ip(text):
    try:
        ip = ipaddress.ip_address(text.split("%")[0].strip("[]"))
    except ValueError:
        return True
    mapped = getattr(ip, "ipv4_mapped", None)
    if mapped is not None:
        ip = mapped
    if (
        ip.is_private
        or ip.is_loopback
        or ip.is_link_local
        or ip.is_multicast
        or ip.is_reserved
        or ip.is_unspecified
    ):
        return True
    if ip.version == 4:
        octets = ip.packed
        if octets[0] == 100 and 64 <= octets[1] <= 127:
            return True
    return not getattr(ip, "is_global", False)


def _addresses(host):
    if host not in _RESOLVED:
        try:
            _RESOLVED[host] = [info[4][0] for info in socket.getaddrinfo(host, None)]
        except OSError:
            _RESOLVED[host] = []
    return _RESOLVED[host]


def _allowed(url):
    try:
        parsed = urlparse(url)
    except ValueError:
        return False
    if parsed.scheme not in ("http", "https"):
        return False
    host = parsed.hostname
    if not host:
        return False
    lower = host.rstrip(".").lower()
    if lower in _BLOCKED_NAMES or lower.endswith(_BLOCKED_SUFFIXES):
        return False
    try:
        ipaddress.ip_address(lower.strip("[]"))
    except ValueError:
        addresses = _addresses(host)
        return bool(addresses) and not any(_blocked_ip(a) for a in addresses)
    return not _blocked_ip(lower)


def _guard(route):
    try:
        allowed = _allowed(route.request.url)
    except Exception:
        allowed = False
    try:
        if allowed:
            route.continue_()
        else:
            route.abort("blockedbyclient")
    except Exception:
        pass


def main():
    params = json.loads(sys.argv[1])
    action = params.get("action")
    headless = params.get("headless", True)
    vw = params.get("viewport", {}).get("width", 1280)
    vh = params.get("viewport", {}).get("height", 720)
    timeout = params.get("timeout", 30000)

    with sync_playwright() as p:
        browser = p.chromium.launch(headless=headless)
        page = browser.new_page(viewport={"width": vw, "height": vh})
        page.set_default_timeout(timeout)
        page.route("**/*", _guard)

        result = {}

        if action == "navigate":
            page.goto(params["url"], wait_until="domcontentloaded")
            result = {"url": page.url, "title": page.title()}

        elif action == "click":
            page.goto(params.get("currentUrl", "about:blank"), wait_until="domcontentloaded")
            page.click(params["selector"])
            result = {"clicked": params["selector"], "url": page.url}

        elif action == "type":
            page.goto(params.get("currentUrl", "about:blank"), wait_until="domcontentloaded")
            page.fill(params["selector"], params["text"])
            result = {"typed": True, "selector": params["selector"]}

        elif action == "screenshot":
            page.goto(params.get("currentUrl", "about:blank"), wait_until="domcontentloaded")
            path = params.get("savePath", "/tmp/screenshot.png")
            page.screenshot(path=path, full_page=params.get("fullPage", False))
            result = {"path": path}

        elif action == "read":
            page.goto(params.get("currentUrl", "about:blank"), wait_until="domcontentloaded")
            text = page.inner_text("body")
            result = {"text": text[:100000], "url": page.url, "title": page.title()}

        elif action == "close":
            result = {"closed": True}

        browser.close()
        print(json.dumps(result))

if __name__ == "__main__":
    main()
"#;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Address ranges a fetched URL must never reach.
///
/// `Ipv4Addr::is_global` / `Ipv6Addr::is_global` are still unstable on the
/// pinned 1.90 toolchain, so the ranges are spelled out. Everything that is not
/// globally routable is refused, which covers the cloud metadata endpoints
/// (169.254.169.254, fd00:ec2::254), RFC1918, CGNAT, loopback and the reserved
/// blocks an attacker can use to reach a service on this host or its network.
fn is_blocked_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => is_blocked_ipv4(ip),
        std::net::IpAddr::V6(ip) => is_blocked_ipv6(ip),
    }
}

fn is_blocked_ipv4(ip: std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0                                   // 0.0.0.0/8 "this network"
        || (a == 100 && (64..=127).contains(&b))    // 100.64.0.0/10 CGNAT
        || (a == 192 && b == 0 && c == 0)           // 192.0.0.0/24 IETF protocol assignments
        || (a == 192 && b == 88 && c == 99)         // 192.88.99.0/24 6to4 relay anycast
        || (a == 198 && (b == 18 || b == 19))       // 198.18.0.0/15 benchmarking
        || a >= 240 // 240.0.0.0/4 reserved
}

fn is_blocked_ipv6(ip: std::net::Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    let segments = ip.segments();
    // fc00::/7 unique local
    if (segments[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // fe80::/10 link-local
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // 2001:db8::/32 documentation
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return true;
    }
    // 100::/64 discard-only
    if segments[0] == 0x0100 && segments[1] == 0 && segments[2] == 0 && segments[3] == 0 {
        return true;
    }
    // ::ffff:0:0/96 (IPv4-mapped) and 64:ff9b::/96 (NAT64) both carry the v4
    // address in the last two groups; 2002::/16 (6to4) carries it in groups 1-2.
    let ipv4_mapped = segments[..5] == [0, 0, 0, 0, 0] && segments[5] == 0xffff;
    let nat64 = segments[0] == 0x0064 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0];
    let embedded_v4 = if ipv4_mapped || nat64 {
        Some(embedded_ipv4(segments[6], segments[7]))
    } else if segments[0] == 0x2002 {
        Some(embedded_ipv4(segments[1], segments[2]))
    } else {
        None
    };
    embedded_v4.is_some_and(is_blocked_ipv4)
}

fn embedded_ipv4(high: u16, low: u16) -> std::net::Ipv4Addr {
    std::net::Ipv4Addr::new(
        (high >> 8) as u8,
        (high & 0xff) as u8,
        (low >> 8) as u8,
        (low & 0xff) as u8,
    )
}

/// Host names that must be refused without asking a resolver at all.
fn is_blocked_host_name(host: &str) -> bool {
    let lower = host.trim_end_matches('.').to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "localhost" | "metadata" | "metadata.google.internal" | "ip6-localhost" | "ip6-loopback"
    ) || lower.ends_with(".localhost")
        || lower.ends_with(".internal")
        || lower.ends_with(".local")
}

/// Verify a resolved address set. Pure, so the whole range table is testable
/// without touching a resolver.
fn assert_addresses_allowed(host: &str, addresses: &[std::net::IpAddr]) -> Result<(), Error> {
    if addresses.is_empty() {
        return Err(Error::Handler(format!(
            "blocked host: {host} did not resolve"
        )));
    }
    for address in addresses {
        if is_blocked_ip(*address) {
            return Err(Error::Handler(format!(
                "blocked host: {host} resolves to {address}"
            )));
        }
    }
    Ok(())
}

/// Resolve a name and check every address it answers with.
///
/// A name that string-matching alone accepts (`whatever.example.com`) can
/// still point at 169.254.169.254 or 127.0.0.1, and a rebinding resolver can
/// answer differently per lookup, so resolution has to happen here. A
/// resolution failure is treated as a block: fail closed.
async fn assert_resolved_host_allowed(host: &str, port: u16) -> Result<(), Error> {
    let addresses: Vec<std::net::IpAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| Error::Handler(format!("blocked host: {host} did not resolve: {e}")))?
        .map(|socket_addr| socket_addr.ip())
        .collect();
    assert_addresses_allowed(host, &addresses)
}

async fn assert_no_ssrf(url_str: &str) -> Result<(), Error> {
    let parsed =
        url::Url::parse(url_str).map_err(|e| Error::Handler(format!("invalid url: {e}")))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(Error::Handler(format!("blocked scheme: {scheme}")));
    }
    match parsed.host() {
        Some(url::Host::Ipv4(ip)) => {
            if is_blocked_ipv4(ip) {
                return Err(Error::Handler(format!("blocked host: {ip}")));
            }
        }
        Some(url::Host::Ipv6(ip)) => {
            if is_blocked_ipv6(ip) {
                return Err(Error::Handler(format!("blocked host: {ip}")));
            }
        }
        Some(url::Host::Domain(host)) => {
            if is_blocked_host_name(host) {
                return Err(Error::Handler(format!("blocked host: {host}")));
            }
            let port = parsed.port_or_known_default().unwrap_or(80);
            assert_resolved_host_allowed(host, port).await?;
        }
        None => return Err(Error::Handler("missing host".into())),
    }
    Ok(())
}

async fn get_session_index(iii: &IIIClient) -> Vec<String> {
    let raw = iii
        .trigger(TriggerRequest {
            function_id: "state::get".into(),
            payload: json!({ "scope": "browser_sessions", "key": "_index" }),
            action: None,
            timeout_ms: None,
        })
        .await
        .ok();
    let Some(v) = raw else { return Vec::new() };
    // Accept both the legacy flat-array shape and the new { ids: [...] } shape
    // produced by the atomic state::update path.
    if let Some(arr) = v.as_array() {
        return arr
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
    }
    if let Some(arr) = v.get("ids").and_then(|x| x.as_array()) {
        return arr
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
    }
    Vec::new()
}

/// Read the post-append session index out of a `state::update` reply.
///
/// The engine answers `{ "new_value": <doc>, "old_value": <doc>, "errors"?: [] }`.
/// `None` means the update did not produce a usable list (an op error, or an
/// `append` onto an absent path, which yields a scalar rather than a list) and
/// the caller must repair the document.
fn reserved_ids(update_reply: &Value, expected_id: &str) -> Option<Vec<String>> {
    if update_reply
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        return None;
    }
    let ids: Vec<String> = update_reply
        .get("new_value")
        .and_then(|value| value.get("ids"))
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|entry| entry.as_str().map(String::from))
        .collect();
    if ids.iter().any(|id| id == expected_id) {
        Some(ids)
    } else {
        None
    }
}

async fn set_session_index(iii: &IIIClient, index: Vec<String>) -> Result<(), Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::set".into(),
        payload: json!({
            "scope": "browser_sessions",
            "key": "_index",
            "value": { "ids": index }
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map(|_| ())
    .map_err(|e| Error::Handler(e.to_string()))
}

async fn load_session(iii: &IIIClient, agent_id: &str) -> Option<BrowserSession> {
    let val = iii
        .trigger(TriggerRequest {
            function_id: "state::get".into(),
            payload: json!({ "scope": "browser_sessions", "key": agent_id }),
            action: None,
            timeout_ms: None,
        })
        .await
        .ok()?;
    if val.is_null() {
        return None;
    }
    serde_json::from_value(val).ok()
}

async fn save_session(iii: &IIIClient, session: &BrowserSession) -> Result<(), Error> {
    let value = serde_json::to_value(session).map_err(|e| Error::Handler(e.to_string()))?;
    iii.trigger(TriggerRequest {
        function_id: "state::set".into(),
        payload: json!({ "scope": "browser_sessions", "key": session.agent_id, "value": value }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map(|_| ())
    .map_err(|e| Error::Handler(e.to_string()))
}

async fn touch_session(iii: &IIIClient, session: &mut BrowserSession) -> Result<(), Error> {
    session.last_activity = now_ms();
    save_session(iii, session).await
}

async fn run_browser_script(
    session: &BrowserSession,
    action: &str,
    extra: Value,
) -> Result<Value, Error> {
    let mut payload = json!({
        "action": action,
        "sessionId": session.id,
        "headless": session.headless,
        "viewport": session.viewport,
        "timeout": DEFAULT_TIMEOUT_MS,
    });
    if let Some(obj) = payload.as_object_mut()
        && let Some(extras) = extra.as_object()
    {
        for (k, v) in extras {
            obj.insert(k.clone(), v.clone());
        }
    }
    let payload_str = serde_json::to_string(&payload).map_err(|e| Error::Handler(e.to_string()))?;

    let timeout = Duration::from_millis(DEFAULT_TIMEOUT_MS + 5_000);

    let child = Command::new("python3")
        .arg(&session.script_path)
        .arg(&payload_str)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| Error::Handler(format!("spawn failed: {e}")))?;

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => result.map_err(|e| Error::Handler(format!("wait failed: {e}")))?,
        Err(_) => {
            // child is dropped on the early return; kill_on_drop guarantees termination.
            return Err(Error::Handler("browser script timed out".into()));
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !stderr.is_empty() && stdout.is_empty() {
        let mut snippet = stderr;
        truncate_to_char_boundary(&mut snippet, 1_000);
        return Err(Error::Handler(format!("Browser error: {snippet}")));
    }

    match serde_json::from_str::<Value>(&stdout) {
        Ok(v) => Ok(v),
        Err(_) => {
            let mut out = stdout;
            truncate_to_char_boundary(&mut out, 100_000);
            Ok(json!({ "output": out }))
        }
    }
}

/// Truncate `s` to at most `max_bytes` bytes, snapping back to the nearest
/// preceding UTF-8 char boundary so multi-byte characters never panic.
fn truncate_to_char_boundary(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

async fn audit(iii: &IIIClient, kind: &str, detail: Value) {
    let payload = json!({ "type": kind, "detail": detail });
    let iii_clone = iii.clone();
    tokio::spawn(async move {
        let _ = iii_clone
            .trigger(TriggerRequest {
                function_id: "security::audit".into(),
                payload,
                action: None,
                timeout_ms: None,
            })
            .await;
    });
}

async fn cleanup_idle_sessions(iii: &IIIClient) {
    let now = now_ms();
    let index = get_session_index(iii).await;
    let mut remaining: Vec<String> = Vec::new();
    for agent_id in index {
        let session = load_session(iii, &agent_id).await;
        match session {
            Some(s) if now - s.last_activity <= IDLE_TIMEOUT_MS => remaining.push(agent_id),
            Some(s) => {
                let _ = tokio::fs::remove_file(&s.script_path).await;
                audit(
                    iii,
                    "browser_idle_cleanup",
                    json!({ "agentId": agent_id, "sessionId": s.id }),
                )
                .await;
                let _ = iii
                    .trigger(TriggerRequest {
                        function_id: "state::set".into(),
                        payload: json!({ "scope": "browser_sessions", "key": agent_id, "value": null }),
                        action: None,
                        timeout_ms: None,
                    })
                    .await;
            }
            None => {
                let _ = iii
                    .trigger(TriggerRequest {
                        function_id: "state::set".into(),
                        payload: json!({ "scope": "browser_sessions", "key": agent_id, "value": null }),
                        action: None,
                        timeout_ms: None,
                    })
                    .await;
            }
        }
    }
    let _ = set_session_index(iii, remaining).await;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, InitOptions::default());

    let iii_bg = iii.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            cleanup_idle_sessions(&iii_bg).await;
        }
    });

    let iii_clone = iii.clone();
    iii.register_function(
        "browser::create_session",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = input.get("body").cloned().unwrap_or(input.clone());
                let agent_id = body["agentId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing agentId".into()))?
                    .to_string();
                let headless = body["headless"].as_bool().unwrap_or(true);
                let viewport: Viewport =
                    serde_json::from_value(body["viewport"].clone()).unwrap_or_default();

                if load_session(&iii, &agent_id).await.is_some() {
                    return Err(Error::Handler(format!(
                        "Session already exists for agent: {agent_id}"
                    )));
                }

                let session_id = uuid::Uuid::new_v4().to_string();
                let script_path =
                    std::env::temp_dir().join(format!("browser-bridge-{session_id}.py"));

                let now = now_ms();
                let session = BrowserSession {
                    id: session_id.clone(),
                    agent_id: agent_id.clone(),
                    current_url: "about:blank".into(),
                    headless,
                    viewport: viewport.clone(),
                    created_at: now,
                    last_activity: now,
                    script_path: script_path.to_string_lossy().to_string(),
                };

                // Atomically reserve a slot in the session index BEFORE doing any I/O.
                // state::update is the engine's CAS-style primitive: we append the
                // agent id and check the resulting size in a single round-trip so
                // two concurrent create_session calls cannot both pass the cap.
                //
                // Protocol, verified against the pinned engine (iii 0.22.1):
                //   * the field is `ops`, not `operations` (`operations` fails the
                //     whole invocation with "missing field `ops`");
                //   * a list element is added with `append`, not `merge` — `merge`
                //     rejects a non-object value and answers 200 with an `errors`
                //     array, i.e. a silent no-op;
                //   * `append` adds ONE element, so the value is the id itself;
                //   * the reply is `{ new_value, old_value }`, not the value.
                let updated_index = iii
                    .trigger(TriggerRequest {
                        function_id: "state::update".into(),
                        payload: json!({
                            "scope": "browser_sessions",
                            "key": "_index",
                            "ops": [
                                { "type": "append", "path": "ids", "value": agent_id.clone() }
                            ],
                        }),
                        action: None,
                        timeout_ms: None,
                    })
                    .await
                    .map_err(|e| Error::Handler(format!("reserve session slot: {e}")))?;
                let ids = reserved_ids(&updated_index, &agent_id);
                let ids: Vec<String> = match ids {
                    Some(ids) => ids,
                    None => {
                        // `append` onto an absent path produces a scalar, not a
                        // list. Repair the index instead of leaving a corrupted
                        // document behind, then continue with a one-entry index.
                        set_session_index(&iii, vec![agent_id.clone()]).await?;
                        vec![agent_id.clone()]
                    }
                };
                if ids.len() > MAX_SESSIONS {
                    // Roll the reservation back.
                    let rolled: Vec<String> =
                        ids.into_iter().filter(|id| id != &agent_id).collect();
                    let _ = iii
                        .trigger(TriggerRequest {
                            function_id: "state::set".into(),
                            payload: json!({
                                "scope": "browser_sessions",
                                "key": "_index",
                                "value": { "ids": rolled }
                            }),
                            action: None,
                            timeout_ms: None,
                        })
                        .await;
                    return Err(Error::Handler(format!(
                        "Max sessions ({MAX_SESSIONS}) reached"
                    )));
                }

                tokio::fs::write(&script_path, BRIDGE_SCRIPT)
                    .await
                    .map_err(|e| Error::Handler(format!("write script failed: {e}")))?;

                save_session(&iii, &session).await?;

                audit(
                    &iii,
                    "browser_session_created",
                    json!({ "agentId": agent_id, "sessionId": session_id, "headless": headless }),
                )
                .await;

                Ok::<Value, Error>(json!({
                    "sessionId": session_id,
                    "agentId": agent_id,
                    "headless": headless,
                    "viewport": viewport,
                }))
            }
        })
        .description("Create a new browser session"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "browser::list_sessions",
        RegisterFunction::new_async(move |_: Value| {
            let iii = iii_clone.clone();
            async move {
                let index = get_session_index(&iii).await;
                let mut list: Vec<Value> = Vec::new();
                let now = now_ms();
                for agent_id in index {
                    if let Some(s) = load_session(&iii, &agent_id).await {
                        list.push(json!({
                            "id": s.id,
                            "agentId": s.agent_id,
                            "currentUrl": s.current_url,
                            "headless": s.headless,
                            "createdAt": s.created_at,
                            "lastActivity": s.last_activity,
                            "idleMs": now - s.last_activity,
                        }));
                    }
                }
                let count = list.len();
                Ok::<Value, Error>(json!({ "sessions": list, "count": count }))
            }
        })
        .description("List active browser sessions"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "browser::navigate",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = input.get("body").cloned().unwrap_or(input.clone());
                let agent_id = body["agentId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing agentId".into()))?;
                let url_str = body["url"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing url".into()))?;
                assert_no_ssrf(url_str).await?;
                let mut session = load_session(&iii, agent_id).await.ok_or_else(|| {
                    Error::Handler(format!("No browser session for agent: {agent_id}"))
                })?;
                touch_session(&iii, &mut session).await?;

                let result =
                    run_browser_script(&session, "navigate", json!({ "url": url_str })).await?;
                let new_url = result["url"].as_str().unwrap_or(url_str).to_string();
                // Re-validate the post-navigation URL to block redirects to internal hosts.
                if new_url != url_str {
                    assert_no_ssrf(&new_url).await?;
                }
                session.current_url = new_url.clone();
                save_session(&iii, &session).await?;

                Ok::<Value, Error>(json!({
                    "url": new_url,
                    "title": result["title"],
                }))
            }
        })
        .description("Navigate to URL with SSRF check"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "browser::click",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = input.get("body").cloned().unwrap_or(input.clone());
                let agent_id = body["agentId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing agentId".into()))?;
                let selector = body["selector"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing selector".into()))?
                    .to_string();
                let mut session = load_session(&iii, agent_id).await.ok_or_else(|| {
                    Error::Handler(format!("No browser session for agent: {agent_id}"))
                })?;
                touch_session(&iii, &mut session).await?;

                // Re-validate the stored URL before reusing it as a navigation target.
                assert_no_ssrf(&session.current_url).await?;
                let result = run_browser_script(
                    &session,
                    "click",
                    json!({ "selector": selector, "currentUrl": session.current_url }),
                )
                .await?;
                if let Some(u) = result["url"].as_str() {
                    let new_url = u.to_string();
                    if new_url != session.current_url {
                        assert_no_ssrf(&new_url).await?;
                    }
                    session.current_url = new_url;
                    save_session(&iii, &session).await?;
                }

                Ok::<Value, Error>(json!({ "clicked": selector, "url": session.current_url }))
            }
        })
        .description("Click element by selector"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "browser::type",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = input.get("body").cloned().unwrap_or(input.clone());
                let agent_id = body["agentId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing agentId".into()))?;
                let selector = body["selector"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing selector".into()))?
                    .to_string();
                let text = body["text"].as_str().unwrap_or("").to_string();
                let mut session = load_session(&iii, agent_id).await.ok_or_else(|| {
                    Error::Handler(format!("No browser session for agent: {agent_id}"))
                })?;
                touch_session(&iii, &mut session).await?;

                // Re-validate the stored URL before reusing it as a navigation target.
                assert_no_ssrf(&session.current_url).await?;
                run_browser_script(
                &session,
                "type",
                json!({ "selector": selector, "text": text, "currentUrl": session.current_url }),
            )
            .await?;

                let len = text.len();
                Ok::<Value, Error>(json!({ "typed": true, "selector": selector, "length": len }))
            }
        })
        .description("Type text into element by selector"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "browser::screenshot",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = input.get("body").cloned().unwrap_or(input.clone());
                let agent_id = body["agentId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing agentId".into()))?;
                let full_page = body["fullPage"].as_bool().unwrap_or(false);
                let mut session = load_session(&iii, agent_id).await.ok_or_else(|| {
                    Error::Handler(format!("No browser session for agent: {agent_id}"))
                })?;
                touch_session(&iii, &mut session).await?;

                let save_path = std::env::temp_dir().join(format!(
                    "screenshot-{}-{}.png",
                    session.id,
                    now_ms()
                ));
                let save_path_str = save_path.to_string_lossy().to_string();

                // Re-validate the stored URL before reusing it as a navigation target.
                assert_no_ssrf(&session.current_url).await?;
                run_browser_script(
                    &session,
                    "screenshot",
                    json!({
                        "currentUrl": session.current_url,
                        "savePath": save_path_str,
                        "fullPage": full_page,
                    }),
                )
                .await?;

                Ok::<Value, Error>(json!({ "path": save_path_str, "url": session.current_url }))
            }
        })
        .description("Take screenshot and save to temp file"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "browser::read_page",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = input.get("body").cloned().unwrap_or(input.clone());
                let agent_id = body["agentId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing agentId".into()))?;
                let mut session = load_session(&iii, agent_id).await.ok_or_else(|| {
                    Error::Handler(format!("No browser session for agent: {agent_id}"))
                })?;
                touch_session(&iii, &mut session).await?;

                // Re-validate the stored URL before reusing it as a navigation target.
                assert_no_ssrf(&session.current_url).await?;
                let result = run_browser_script(
                    &session,
                    "read",
                    json!({ "currentUrl": session.current_url }),
                )
                .await?;

                let mut text = result["text"].as_str().unwrap_or("").to_string();
                truncate_to_char_boundary(&mut text, 100_000);
                Ok::<Value, Error>(json!({
                    "text": text,
                    "url": result["url"].as_str().unwrap_or(&session.current_url),
                    "title": result["title"].as_str().unwrap_or(""),
                }))
            }
        })
        .description("Extract page text content"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "browser::close",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let body = input.get("body").cloned().unwrap_or(input.clone());
                let agent_id = body["agentId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing agentId".into()))?;
                let session = load_session(&iii, agent_id).await.ok_or_else(|| {
                    Error::Handler(format!("No browser session for agent: {agent_id}"))
                })?;

                let _ = iii
                .trigger(TriggerRequest {
                    function_id: "state::set".into(),
                    payload: json!({ "scope": "browser_sessions", "key": agent_id, "value": null }),
                    action: None,
                    timeout_ms: None,
                })
                .await;
                let index = get_session_index(&iii).await;
                let new_index: Vec<String> =
                    index.into_iter().filter(|id| id != agent_id).collect();
                set_session_index(&iii, new_index).await?;
                let _ = tokio::fs::remove_file(&session.script_path).await;

                audit(
                    &iii,
                    "browser_session_closed",
                    json!({ "agentId": agent_id, "sessionId": session.id }),
                )
                .await;

                Ok::<Value, Error>(json!({
                    "closed": true,
                    "agentId": agent_id,
                    "sessionId": session.id,
                }))
            }
        })
        .description("Close browser session"),
    );

    let triggers = [
        ("browser::create_session", "POST", "api/browser/session"),
        ("browser::list_sessions", "GET", "api/browser/sessions"),
        ("browser::navigate", "POST", "api/browser/navigate"),
        ("browser::click", "POST", "api/browser/click"),
        ("browser::type", "POST", "api/browser/type"),
        ("browser::screenshot", "POST", "api/browser/screenshot"),
        ("browser::read_page", "POST", "api/browser/read"),
        ("browser::close", "POST", "api/browser/close"),
    ];
    for (fid, method, path) in triggers {
        agentos_http_adapter::register_http_trigger(
            &iii,
            fid.to_string(),
            json!({ "http_method": method, "api_path": path }),
            None,
        )?;
    }

    tracing::info!("browser worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod state_protocol_tests {
    use super::reserved_ids;
    use serde_json::json;

    #[test]
    fn reserved_ids_reads_the_update_envelope() {
        let reply = json!({
            "new_value": { "ids": ["agent-1", "agent-2"] },
            "old_value": { "ids": ["agent-1"] },
        });
        assert_eq!(
            reserved_ids(&reply, "agent-2"),
            Some(vec!["agent-1".to_string(), "agent-2".to_string()])
        );
    }

    #[test]
    fn reserved_ids_rejects_a_reply_that_is_not_the_envelope() {
        // The value itself, which is what the old code read.
        let reply = json!({ "ids": ["agent-1"] });
        assert_eq!(reserved_ids(&reply, "agent-1"), None);
    }

    #[test]
    fn reserved_ids_rejects_op_errors() {
        let reply = json!({
            "errors": [{ "code": "merge.value.not_an_object" }],
            "new_value": { "ids": ["agent-1"] },
        });
        assert_eq!(reserved_ids(&reply, "agent-1"), None);
    }

    #[test]
    fn reserved_ids_rejects_a_scalar_produced_by_append_on_a_missing_path() {
        // `append` onto an absent path concatenates strings instead of building
        // a list; the caller has to repair the document.
        let reply = json!({ "new_value": { "ids": "agent-1agent-2" }, "old_value": null });
        assert_eq!(reserved_ids(&reply, "agent-2"), None);
    }

    #[test]
    fn reserved_ids_rejects_a_list_without_our_reservation() {
        let reply = json!({ "new_value": { "ids": ["someone-else"] } });
        assert_eq!(reserved_ids(&reply, "agent-1"), None);
    }
}

#[cfg(test)]
mod ssrf_tests {
    use super::{
        BRIDGE_SCRIPT, assert_addresses_allowed, assert_no_ssrf, assert_resolved_host_allowed,
        is_blocked_host_name, is_blocked_ip,
    };
    use std::net::IpAddr;

    fn ip(text: &str) -> IpAddr {
        text.parse().expect("test address")
    }

    #[test]
    fn blocks_every_non_global_v4_range() {
        for address in [
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.1",
            "169.254.169.254",
            "169.254.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(is_blocked_ip(ip(address)), "{address} must be blocked");
        }
    }

    #[test]
    fn allows_ordinary_public_v4() {
        for address in [
            "93.184.215.14",
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "100.128.0.1",
        ] {
            assert!(!is_blocked_ip(ip(address)), "{address} must be allowed");
        }
    }

    #[test]
    fn blocks_every_non_global_v6_range() {
        for address in [
            "::1",
            "::",
            "fc00::1",
            "fd00:ec2::254",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "100::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
        ] {
            assert!(is_blocked_ip(ip(address)), "{address} must be blocked");
        }
    }

    #[test]
    fn allows_ordinary_public_v6() {
        for address in [
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_blocked_ip(ip(address)), "{address} must be allowed");
        }
    }

    #[test]
    fn resolved_set_is_rejected_if_any_address_is_internal() {
        // This is the DNS-rebinding / split-horizon case the old string check
        // could not see: the name looks ordinary, the answer does not.
        let error = assert_addresses_allowed(
            "attacker.example.com",
            &[ip("93.184.215.14"), ip("169.254.169.254")],
        )
        .expect_err("a mixed answer must be refused")
        .to_string();
        assert!(error.contains("169.254.169.254"), "{error}");

        assert!(
            assert_addresses_allowed("attacker.example.com", &[ip("169.254.169.254")]).is_err()
        );
        assert!(assert_addresses_allowed("ok.example.com", &[ip("93.184.215.14")]).is_ok());
    }

    #[test]
    fn an_empty_answer_fails_closed() {
        let error = assert_addresses_allowed("nothing.example.com", &[])
            .expect_err("no answer must be a block")
            .to_string();
        assert!(error.contains("did not resolve"), "{error}");
    }

    #[test]
    fn internal_name_suffixes_are_refused_without_a_resolver() {
        for host in [
            "localhost",
            "LOCALHOST",
            "localhost.",
            "api.localhost",
            "metadata",
            "metadata.google.internal",
            "ip6-localhost",
            "printer.local",
            "db.internal",
        ] {
            assert!(is_blocked_host_name(host), "{host} must be blocked");
        }
        for host in [
            "example.com",
            "api.example.org",
            "internal-tools.example.com",
        ] {
            assert!(!is_blocked_host_name(host), "{host} must be allowed");
        }
    }

    #[tokio::test]
    async fn ip_literals_are_blocked_without_dns() {
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:3111/api/security",
            "http://[::1]/",
            "http://10.0.0.1/",
            "http://[::ffff:127.0.0.1]/",
            "http://0.0.0.0/",
        ] {
            assert!(assert_no_ssrf(url).await.is_err(), "{url} must be refused");
        }
    }

    #[tokio::test]
    async fn non_http_schemes_are_blocked() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/",
            "gopher://example.com/",
        ] {
            let error = assert_no_ssrf(url)
                .await
                .expect_err("must be refused")
                .to_string();
            assert!(
                error.contains("blocked scheme") || error.contains("invalid url"),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn known_internal_names_are_blocked() {
        for url in [
            "http://localhost:3111/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://api.localhost/",
        ] {
            assert!(assert_no_ssrf(url).await.is_err(), "{url} must be refused");
        }
    }

    /// Exercises the real resolver on a name that the static list does not
    /// cover, proving resolution actually happens. `localhost` is in every
    /// host's `/etc/hosts`, so this needs no network.
    #[tokio::test]
    async fn the_resolver_path_blocks_a_name_that_maps_to_loopback() {
        let error = assert_resolved_host_allowed("localhost", 80)
            .await
            .expect_err("localhost resolves to a loopback address")
            .to_string();
        assert!(error.contains("blocked host"), "{error}");
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_fails_closed() {
        assert!(
            assert_no_ssrf("http://no-such-host-9c3a.invalid/")
                .await
                .is_err(),
            "an unresolvable name must be refused, not allowed"
        );
    }

    #[test]
    fn the_bridge_script_installs_the_request_guard() {
        // Redirects and sub-resources are only checked inside the browser, so
        // losing this line silently reopens post-redirect SSRF.
        assert!(BRIDGE_SCRIPT.contains(r#"page.route("**/*", _guard)"#));
        assert!(BRIDGE_SCRIPT.contains(r#"route.abort("blockedbyclient")"#));
        assert!(BRIDGE_SCRIPT.contains("def _allowed(url):"));
    }
}
