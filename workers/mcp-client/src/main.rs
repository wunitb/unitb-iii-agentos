use dashmap::DashMap;
use iii_sdk::errors::Error;
use iii_sdk::{
    IIIClient, InitOptions, RegisterFunction, protocol::TriggerRequest, register_worker,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;
use tokio::sync::oneshot;

const RPC_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct McpTool {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "inputSchema", default)]
    input_schema: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct IntegrationDocument {
    integration: IntegrationManifest,
}

#[derive(Debug, Clone, Deserialize)]
struct IntegrationManifest {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    category: String,
    transport: String,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    url: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, IntegrationEnv>,
}

#[derive(Debug, Clone, Deserialize)]
struct IntegrationEnv {
    #[serde(default)]
    required: bool,
}

struct McpConnection {
    id: String,
    name: String,
    transport: String,
    tools: Mutex<Vec<McpTool>>,
    capabilities: Mutex<Value>,
    connected_at: i64,
    next_rpc_id: AtomicI64,
    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Option<Child>>,
    pending: DashMap<i64, oneshot::Sender<Result<Value, String>>>,
}

#[derive(Default)]
struct State {
    connections: DashMap<String, Arc<McpConnection>>,
    serve: Mutex<Option<ServeRefs>>,
}

struct ServeRefs {
    handler: iii_sdk::runtime::FunctionRef,
    trigger: iii_sdk::trigger::Trigger,
}

fn safe_env() -> Vec<(String, String)> {
    let allow = ["PATH", "HOME", "USER", "LANG", "TERM", "SHELL"];
    let mut out = Vec::new();
    for k in allow {
        if let Ok(v) = std::env::var(k) {
            out.push((k.to_string(), v));
        }
    }
    out
}

fn validate_command(cmd: &str) -> Result<(), Error> {
    if cmd.is_empty()
        || cmd.contains(';')
        || cmd.contains('|')
        || cmd.contains('&')
        || cmd.contains('`')
    {
        return Err(Error::Handler("invalid mcp command".into()));
    }
    Ok(())
}

fn validate_env_key(key: &str) -> Result<(), Error> {
    if key.is_empty()
        || !key
            .bytes()
            .all(|byte| byte == b'_' || byte.is_ascii_uppercase() || byte.is_ascii_digit())
    {
        return Err(Error::Handler(format!("invalid environment key: {key}")));
    }
    Ok(())
}

fn requested_env(body: &Value) -> Result<Vec<(String, String)>, Error> {
    body.get("env")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(key, value)| {
            validate_env_key(key)?;
            let value = value.as_str().ok_or_else(|| {
                Error::Handler(format!("environment value for {key} must be a string"))
            })?;
            Ok((key.clone(), value.to_string()))
        })
        .collect()
}

fn valid_integration_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn integration_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path) = std::env::var_os("AGENTOS_INTEGRATIONS_DIR") {
        dirs.push(PathBuf::from(path));
    }
    if let Some(home) = std::env::var_os("AGENTOS_HOME") {
        dirs.push(PathBuf::from(home).join("runtime/integrations"));
    }
    if let Ok(current) = std::env::current_dir() {
        dirs.push(current.join("integrations"));
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        dirs.push(parent.join("integrations"));
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

fn parse_integration(path: &Path) -> Result<IntegrationManifest, Error> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| Error::Handler(format!("read {}: {error}", path.display())))?;
    let document: IntegrationDocument = toml::from_str(&source)
        .map_err(|error| Error::Handler(format!("parse {}: {error}", path.display())))?;
    if !valid_integration_id(&document.integration.id) {
        return Err(Error::Handler(format!(
            "invalid integration id in {}",
            path.display()
        )));
    }
    Ok(document.integration)
}

fn integration_manifests() -> Result<Vec<IntegrationManifest>, Error> {
    let mut files = Vec::new();
    for directory in integration_dirs().into_iter().filter(|path| path.is_dir()) {
        for entry in std::fs::read_dir(&directory)
            .map_err(|error| Error::Handler(format!("read {}: {error}", directory.display())))?
        {
            let path = entry
                .map_err(|error| Error::Handler(error.to_string()))?
                .path();
            if path.extension().and_then(|extension| extension.to_str()) == Some("toml") {
                files.push(path);
            }
        }
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        return Err(Error::Handler(
            "integration manifests not found".to_string(),
        ));
    }

    let mut manifests = BTreeMap::new();
    for path in files {
        let manifest = parse_integration(&path)?;
        manifests.entry(manifest.id.clone()).or_insert(manifest);
    }
    Ok(manifests.into_values().collect())
}

fn integration_manifest(id: &str) -> Result<IntegrationManifest, Error> {
    if !valid_integration_id(id) {
        return Err(Error::Handler(format!("invalid integration id: {id}")));
    }
    integration_manifests()?
        .into_iter()
        .find(|manifest| manifest.id == id)
        .ok_or_else(|| Error::Handler(format!("integration not found: {id}")))
}

fn integration_environment(
    manifest: &IntegrationManifest,
    body: &Value,
) -> Result<serde_json::Map<String, Value>, Error> {
    let provided = body.get("env").and_then(Value::as_object);
    if let Some(provided) = provided {
        for key in provided.keys() {
            if !manifest.env.contains_key(key) {
                return Err(Error::Handler(format!(
                    "environment key {key} is not declared by integration {}",
                    manifest.id
                )));
            }
        }
    }

    let mut shortcut_key = body.get("key").and_then(Value::as_str);
    let mut environment = serde_json::Map::new();
    for (key, spec) in &manifest.env {
        let value = provided
            .and_then(|values| values.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| std::env::var(key).ok())
            .or_else(|| {
                if spec.required {
                    shortcut_key.take().map(str::to_string)
                } else {
                    None
                }
            });
        match value {
            Some(value) => {
                environment.insert(key.clone(), json!(value));
            }
            None if spec.required => {
                return Err(Error::Handler(format!(
                    "integration {} requires environment key {key}",
                    manifest.id
                )));
            }
            None => {}
        }
    }
    Ok(environment)
}

async fn list_integrations(state: Arc<State>, input: Value) -> Result<Value, Error> {
    let query = input
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let integrations = integration_manifests()?
        .into_iter()
        .filter(|manifest| {
            query.is_empty()
                || manifest.id.to_ascii_lowercase().contains(&query)
                || manifest.name.to_ascii_lowercase().contains(&query)
                || manifest.description.to_ascii_lowercase().contains(&query)
        })
        .map(|manifest| {
            let status = if state.connections.contains_key(&manifest.id) {
                "active"
            } else {
                "inactive"
            };
            json!({
                "id": manifest.id,
                "name": manifest.name,
                "type": manifest.transport,
                "category": manifest.category,
                "status": status,
                "description": manifest.description,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!(integrations))
}

async fn add_integration(state: Arc<State>, iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let body = input.get("body").cloned().unwrap_or(input);
    let id = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("name is required".to_string()))?;
    let manifest = integration_manifest(id)?;
    let environment = integration_environment(&manifest, &body)?;
    connect(
        state,
        iii,
        json!({
            "name": manifest.id,
            "transport": manifest.transport,
            "command": manifest.command,
            "args": manifest.args,
            "url": manifest.url,
            "env": environment,
        }),
    )
    .await
}

async fn remove_integration(
    state: Arc<State>,
    iii: &IIIClient,
    input: Value,
) -> Result<Value, Error> {
    let name = input
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Handler("name is required".to_string()))?;
    disconnect(state, iii, json!({ "name": name })).await
}

async fn send_rpc(conn: &McpConnection, method: &str, params: Value) -> Result<Value, Error> {
    let id = conn.next_rpc_id.fetch_add(1, Ordering::SeqCst);
    let message = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });

    let (tx, rx) = oneshot::channel();
    conn.pending.insert(id, tx);

    {
        let mut stdin_guard = conn.stdin.lock().await;
        if let Some(stdin) = stdin_guard.as_mut() {
            let mut bytes =
                serde_json::to_vec(&message).map_err(|e| Error::Handler(e.to_string()))?;
            bytes.push(b'\n');
            stdin
                .write_all(&bytes)
                .await
                .map_err(|e| Error::Handler(format!("stdin write failed: {e}")))?;
        } else {
            conn.pending.remove(&id);
            return Err(Error::Handler("connection has no stdin".into()));
        }
    }

    match tokio::time::timeout(Duration::from_millis(RPC_TIMEOUT_MS), rx).await {
        Ok(Ok(Ok(v))) => Ok(v),
        Ok(Ok(Err(e))) => Err(Error::Handler(e)),
        Ok(Err(_)) => Err(Error::Handler("rpc channel dropped".into())),
        Err(_) => {
            conn.pending.remove(&id);
            Err(Error::Handler(format!("RPC timeout: {method}")))
        }
    }
}

fn handle_rpc_response(conn: &McpConnection, msg: &Value) {
    let id = match msg.get("id").and_then(|v| v.as_i64()) {
        Some(i) => i,
        None => return,
    };
    let Some((_, tx)) = conn.pending.remove(&id) else {
        return;
    };
    if let Some(err) = msg.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("error");
        let _ = tx.send(Err(format!("RPC error {code}: {message}")));
    } else {
        let _ = tx.send(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));
    }
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

async fn connect(state: Arc<State>, iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let body = input.get("body").cloned().unwrap_or(input.clone());
    let name = body["name"]
        .as_str()
        .ok_or_else(|| Error::Handler("missing name".into()))?
        .to_string();
    let transport = body["transport"].as_str().unwrap_or("stdio").to_string();
    let command = body["command"].as_str().map(String::from);
    let args: Option<Vec<String>> = body["args"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });
    let url = body["url"].as_str().map(String::from);
    let requested_env = requested_env(&body)?;

    if state.connections.contains_key(&name) {
        return Err(Error::Handler(format!(
            "Connection '{name}' already exists"
        )));
    }

    let conn = Arc::new(McpConnection {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.clone(),
        transport: transport.clone(),
        tools: Mutex::new(Vec::new()),
        capabilities: Mutex::new(Value::Null),
        connected_at: chrono::Utc::now().timestamp_millis(),
        next_rpc_id: AtomicI64::new(1),
        stdin: Mutex::new(None),
        child: Mutex::new(None),
        pending: DashMap::new(),
    });

    if transport == "stdio" {
        let cmd_str = command
            .as_deref()
            .ok_or_else(|| Error::Handler("stdio transport requires command".into()))?;
        validate_command(cmd_str)?;

        let mut child_cmd = Command::new(cmd_str);
        if let Some(ref a) = args {
            child_cmd.args(a);
        }
        child_cmd.env_clear();
        for (k, v) in safe_env() {
            child_cmd.env(k, v);
        }
        for (key, value) in requested_env {
            child_cmd.env(key, value);
        }
        child_cmd.stdin(std::process::Stdio::piped());
        child_cmd.stdout(std::process::Stdio::piped());
        // Pipe + drain stderr explicitly so a chatty MCP server can't fill the
        // pipe buffer and stall the session by blocking the writer.
        child_cmd.stderr(std::process::Stdio::piped());
        child_cmd.kill_on_drop(true);

        let mut child = child_cmd
            .spawn()
            .map_err(|e| Error::Handler(format!("spawn failed: {e}")))?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        *conn.stdin.lock().await = stdin;

        if let Some(stdout) = stdout {
            let conn_for_reader = conn.clone();
            let state_for_reader = state.clone();
            let iii_for_reader = iii.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    if let Ok(msg) = serde_json::from_str::<Value>(&line)
                        && msg.get("jsonrpc").and_then(|v| v.as_str()) == Some("2.0")
                    {
                        handle_rpc_response(&conn_for_reader, &msg);
                    }
                }
                state_for_reader.connections.remove(&conn_for_reader.name);
                audit(
                    &iii_for_reader,
                    "mcp_disconnect",
                    json!({ "name": conn_for_reader.name, "reason": "stdout_closed" }),
                )
                .await;
            });
        }

        if let Some(stderr) = stderr {
            let server_name = conn.name.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    tracing::debug!(server = %server_name, "mcp stderr: {line}");
                }
            });
        }

        *conn.child.lock().await = Some(child);
    } else if transport == "sse" {
        // SSE transport is a placeholder in this port — the stdin-based
        // handshake below would always fail with "connection has no stdin",
        // leaving a dead entry in state.connections. Return early instead.
        if url.is_none() {
            return Err(Error::Handler("SSE transport requires url".into()));
        }
        return Err(Error::Handler(
            "SSE transport is not yet implemented".into(),
        ));
    } else {
        return Err(Error::Handler(format!(
            "unsupported transport: {transport}"
        )));
    }

    let init_result = send_rpc(
        &conn,
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "clientInfo": { "name": "agentos", "version": env!("CARGO_PKG_VERSION") },
        }),
    )
    .await?;

    *conn.capabilities.lock().await = init_result
        .get("capabilities")
        .cloned()
        .unwrap_or(Value::Null);

    let tools_result = send_rpc(&conn, "tools/list", json!({})).await?;
    let tools: Vec<McpTool> = tools_result
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| serde_json::from_value(t.clone()).ok())
                .collect()
        })
        .unwrap_or_default();

    let tool_count = tools.len();
    *conn.tools.lock().await = tools;

    // Only insert AFTER initialize + tools/list succeed so a partial failure
    // never leaves a dead handle behind for a future call to find.
    state.connections.insert(name.clone(), conn.clone());

    let _ = iii
        .trigger(TriggerRequest {
            function_id: "state::set".into(),
            payload: json!({
                "scope": "mcp_connections",
                "key": &name,
                "value": {
                    "id": conn.id,
                    "name": &name,
                    "transport": &transport,
                    "command": command,
                    "url": url,
                    "toolCount": tool_count,
                    "connectedAt": conn.connected_at,
                },
            }),
            action: None,
            timeout_ms: None,
        })
        .await;

    audit(
        iii,
        "mcp_connect",
        json!({ "name": &name, "transport": &transport, "toolCount": tool_count }),
    )
    .await;

    Ok(json!({
        "connected": true,
        "name": name,
        "tools": tool_count,
        "capabilities": *conn.capabilities.lock().await,
    }))
}

async fn disconnect(state: Arc<State>, iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let body = input.get("body").cloned().unwrap_or(input.clone());
    let name = body["name"]
        .as_str()
        .ok_or_else(|| Error::Handler("missing name".into()))?
        .to_string();
    let conn = state
        .connections
        .remove(&name)
        .map(|(_, v)| v)
        .ok_or_else(|| Error::Handler(format!("No connection '{name}'")))?;

    if let Some(mut child) = conn.child.lock().await.take() {
        let _ = child.kill().await;
    }

    conn.pending.clear();

    let _ = iii
        .trigger(TriggerRequest {
            function_id: "state::delete".into(),
            payload: json!({ "scope": "mcp_connections", "key": &name }),
            action: None,
            timeout_ms: None,
        })
        .await;

    Ok(json!({ "disconnected": true, "name": name }))
}

async fn list_tools(state: Arc<State>) -> Result<Value, Error> {
    let mut tools = Vec::new();
    for entry in state.connections.iter() {
        let server = entry.key().clone();
        let conn = entry.value().clone();
        let conn_tools = conn.tools.lock().await.clone();
        for tool in conn_tools {
            tools.push(json!({
                "server": &server,
                "name": &tool.name,
                "namespaced": format!("mcp_{server}_{}", tool.name),
                "description": &tool.description,
            }));
        }
    }
    let count = tools.len();
    Ok(json!({ "tools": tools, "count": count }))
}

async fn call_tool(state: Arc<State>, iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let body = input.get("body").cloned().unwrap_or(input.clone());
    let server = body["server"]
        .as_str()
        .ok_or_else(|| Error::Handler("missing server".into()))?
        .to_string();
    let tool = body["tool"]
        .as_str()
        .ok_or_else(|| Error::Handler("missing tool".into()))?
        .to_string();
    let tool_args = body.get("arguments").cloned().unwrap_or(json!({}));

    let conn = state
        .connections
        .get(&server)
        .map(|r| r.clone())
        .ok_or_else(|| Error::Handler(format!("No connection '{server}'")))?;

    let exists = conn.tools.lock().await.iter().any(|t| t.name == tool);
    if !exists {
        return Err(Error::Handler(format!(
            "Tool '{tool}' not found on server '{server}'"
        )));
    }

    let result = send_rpc(
        &conn,
        "tools/call",
        json!({ "name": &tool, "arguments": tool_args }),
    )
    .await?;

    audit(
        iii,
        "mcp_tool_call",
        json!({ "server": &server, "tool": &tool }),
    )
    .await;

    Ok(result)
}

async fn list_connections(state: Arc<State>) -> Result<Value, Error> {
    let now = chrono::Utc::now().timestamp_millis();
    let mut list = Vec::new();
    for entry in state.connections.iter() {
        let conn = entry.value().clone();
        let tool_count = conn.tools.lock().await.len();
        list.push(json!({
            "id": conn.id,
            "name": conn.name,
            "transport": conn.transport,
            "toolCount": tool_count,
            "connectedAt": conn.connected_at,
            "uptime": now - conn.connected_at,
        }));
    }
    let count = list.len();
    Ok(json!({ "connections": list, "count": count }))
}

async fn serve(state: Arc<State>, iii: &IIIClient, input: Value) -> Result<Value, Error> {
    let body = input.get("body").cloned().unwrap_or(input.clone());
    let exposed_tools = body
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let count = exposed_tools.len();

    let tools_for_handler = exposed_tools.clone();
    let iii_for_handler = iii.clone();

    let handler_ref = iii.register_function("mcp::serve_handler", RegisterFunction::new_async(move |req: Value| {
        let tools = tools_for_handler.clone();
        let iii = iii_for_handler.clone();
        async move {
            let msg = req.get("body").cloned().unwrap_or(req.clone());
            let method = msg.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let id = msg.get("id").cloned().unwrap_or(Value::Null);

            if method == "initialize" {
                return Ok::<Value, Error>(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "agentos", "version": env!("CARGO_PKG_VERSION") },
                    },
                }));
            }

            if method == "tools/list" {
                let tools_out: Vec<Value> = tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.get("name").cloned().unwrap_or(Value::Null),
                            "description": t.get("description").cloned().unwrap_or(Value::Null),
                            "inputSchema": t.get("inputSchema").cloned().unwrap_or(Value::Null),
                        })
                    })
                    .collect();
                return Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "tools": tools_out },
                }));
            }

            if method == "tools/call" {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let tool = tools
                    .iter()
                    .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(tool_name))
                    .cloned();
                let Some(tool) = tool else {
                    return Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": format!("Tool not found: {tool_name}") },
                    }));
                };
                let function_id = tool
                    .get("functionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

                match iii
                    .trigger(TriggerRequest {
                        function_id: function_id.clone(),
                        payload: arguments,
                        action: None,
                        timeout_ms: None,
                    })
                    .await
                {
                    Ok(result) => Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": serde_json::to_string(&result).unwrap_or_default() }],
                        },
                    })),
                    Err(e) => Ok(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": e.to_string() }],
                            "isError": true,
                        },
                    })),
                }
            } else {
                Ok(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" },
                }))
            }
        }
    }).description("Handle incoming MCP JSON-RPC requests"));

    let trigger_ref = agentos_http_adapter::register_http_trigger(
        &iii,
        "mcp::serve_handler",
        json!({ "http_method": "POST", "api_path": "mcp/rpc" }),
        None,
    )?;

    {
        let mut guard = state.serve.lock().await;
        if let Some(prev) = guard.take() {
            prev.handler.unregister();
            prev.trigger.unregister();
        }
        *guard = Some(ServeRefs {
            handler: handler_ref,
            trigger: trigger_ref,
        });
    }

    Ok(json!({ "serving": true, "tools": count }))
}

async fn unserve(state: Arc<State>) -> Result<Value, Error> {
    let mut guard = state.serve.lock().await;
    if let Some(refs) = guard.take() {
        refs.handler.unregister();
        refs.trigger.unregister();
        Ok(json!({ "unserved": true }))
    } else {
        Ok(json!({ "unserved": false, "reason": "not serving" }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, InitOptions::default());
    let state = Arc::new(State::default());

    {
        let state = state.clone();
        let iii_for_fn = iii.clone();
        iii.register_function(
            "mcp::connect",
            RegisterFunction::new_async(move |input: Value| {
                let state = state.clone();
                let iii = iii_for_fn.clone();
                async move { connect(state, &iii, input).await }
            })
            .description("Connect to an MCP server via stdio or SSE"),
        );
    }

    {
        let state = state.clone();
        let iii_for_fn = iii.clone();
        iii.register_function(
            "mcp::disconnect",
            RegisterFunction::new_async(move |input: Value| {
                let state = state.clone();
                let iii = iii_for_fn.clone();
                async move { disconnect(state, &iii, input).await }
            })
            .description("Disconnect from an MCP server"),
        );
    }

    {
        let state = state.clone();
        iii.register_function(
            "mcp::list_tools",
            RegisterFunction::new_async(move |_: Value| {
                let state = state.clone();
                async move { list_tools(state).await }
            })
            .description("List tools from connected MCP servers"),
        );
    }

    {
        let state = state.clone();
        let iii_for_fn = iii.clone();
        iii.register_function(
            "mcp::call_tool",
            RegisterFunction::new_async(move |input: Value| {
                let state = state.clone();
                let iii = iii_for_fn.clone();
                async move { call_tool(state, &iii, input).await }
            })
            .description("Call a tool on a connected MCP server"),
        );
    }

    {
        let state = state.clone();
        iii.register_function(
            "mcp::list_connections",
            RegisterFunction::new_async(move |_: Value| {
                let state = state.clone();
                async move { list_connections(state).await }
            })
            .description("List active MCP connections"),
        );
    }

    {
        let state = state.clone();
        let iii_for_fn = iii.clone();
        iii.register_function(
            "mcp::serve",
            RegisterFunction::new_async(move |input: Value| {
                let state = state.clone();
                let iii = iii_for_fn.clone();
                async move { serve(state, &iii, input).await }
            })
            .description("Register agentos as an MCP server exposing agent functions"),
        );
    }

    {
        let state = state.clone();
        iii.register_function(
            "mcp::unserve",
            RegisterFunction::new_async(move |_: Value| {
                let state = state.clone();
                async move { unserve(state).await }
            })
            .description("Unregister the MCP serve handler and its HTTP trigger"),
        );
    }

    {
        let state = state.clone();
        iii.register_function(
            "integration::list",
            RegisterFunction::new_async(move |input: Value| {
                let state = state.clone();
                async move { list_integrations(state, input).await }
            })
            .description("List available integration manifests"),
        );
    }

    {
        let state = state.clone();
        let iii_for_fn = iii.clone();
        iii.register_function(
            "integration::add",
            RegisterFunction::new_async(move |input: Value| {
                let state = state.clone();
                let iii = iii_for_fn.clone();
                async move { add_integration(state, &iii, input).await }
            })
            .description("Connect an integration from its manifest"),
        );
    }

    {
        let state = state.clone();
        let iii_for_fn = iii.clone();
        iii.register_function(
            "integration::remove",
            RegisterFunction::new_async(move |input: Value| {
                let state = state.clone();
                let iii = iii_for_fn.clone();
                async move { remove_integration(state, &iii, input).await }
            })
            .description("Disconnect an active integration"),
        );
    }

    let triggers = [
        ("mcp::connect", "POST", "api/mcp/connect"),
        ("mcp::disconnect", "POST", "api/mcp/disconnect"),
        ("mcp::list_tools", "GET", "api/mcp/tools"),
        ("mcp::call_tool", "POST", "api/mcp/call"),
        ("mcp::list_connections", "GET", "api/mcp/connections"),
        ("mcp::serve", "POST", "api/mcp/serve"),
        ("mcp::unserve", "POST", "api/mcp/unserve"),
        ("integration::list", "GET", "api/integrations"),
        ("integration::add", "POST", "api/integrations"),
        ("integration::remove", "DELETE", "api/integrations/{name}"),
    ];
    for (fid, method, path) in triggers {
        agentos_http_adapter::register_http_trigger(
            &iii,
            fid.to_string(),
            json!({ "http_method": method, "api_path": path }),
            None,
        )?;
    }

    tracing::info!("mcp-client worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn test_manifest() -> IntegrationManifest {
        IntegrationManifest {
            id: "test".to_string(),
            name: "Test".to_string(),
            description: String::new(),
            category: String::new(),
            transport: "stdio".to_string(),
            command: Some("test".to_string()),
            args: Vec::new(),
            url: None,
            env: BTreeMap::from([
                (
                    "AGENTOS_TEST_REQUIRED_9C3A".to_string(),
                    IntegrationEnv { required: true },
                ),
                (
                    "AGENTOS_TEST_OPTIONAL_9C3A".to_string(),
                    IntegrationEnv { required: false },
                ),
            ]),
        }
    }

    #[test]
    fn repository_integration_manifests_are_valid_and_unique() {
        let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../integrations");
        let mut ids = BTreeSet::new();
        let mut count = 0;
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
                continue;
            }
            let manifest = parse_integration(&path).unwrap();
            assert_eq!(
                path.file_stem().and_then(|stem| stem.to_str()),
                Some(manifest.id.as_str())
            );
            assert!(ids.insert(manifest.id));
            count += 1;
        }
        assert!(count > 0);
    }

    #[test]
    fn integration_environment_accepts_declared_key_shortcut() {
        let environment =
            integration_environment(&test_manifest(), &json!({ "key": "secret-value" })).unwrap();
        assert_eq!(
            environment.get("AGENTOS_TEST_REQUIRED_9C3A"),
            Some(&json!("secret-value"))
        );
        assert!(!environment.contains_key("AGENTOS_TEST_OPTIONAL_9C3A"));
    }

    #[test]
    fn integration_environment_rejects_undeclared_keys() {
        let error = integration_environment(
            &test_manifest(),
            &json!({ "env": { "UNDECLARED": "secret" } }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("is not declared"));
    }
}
