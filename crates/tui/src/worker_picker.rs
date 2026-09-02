use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerCard {
    pub name: String,
    pub description: String,
    pub functions: Vec<String>,
    pub installed: bool,
    pub binary_path: Option<String>,
}

#[allow(dead_code)]
pub fn parse_catalog(api_response: &Value, installed: &[String]) -> Vec<WorkerCard> {
    let arr = match api_response.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    let installed_set: std::collections::HashSet<&str> =
        installed.iter().map(|s| s.as_str()).collect();
    arr.iter()
        .filter_map(|v| {
            let name = v.get("name").and_then(|s| s.as_str())?.to_string();
            let description = v
                .get("description")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let functions: Vec<String> = v
                .get("functions")
                .and_then(|f| f.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let installed = installed_set.contains(name.as_str());
            Some(WorkerCard {
                name,
                description,
                functions,
                installed,
                binary_path: v
                    .get("binary_path")
                    .and_then(|s| s.as_str())
                    .map(String::from),
            })
        })
        .collect()
}

pub fn builtin_catalog() -> Vec<WorkerCard> {
    // Every function id below is registered by that worker in `workers/<name>`.
    // The table is hardcoded, so `builtin_catalog_only_advertises_registered_functions`
    // re-derives the real ids from `workers/**` and fails when a row drifts.
    const ENTRIES: &[(&str, &str, &[&str])] = &[
        (
            "memory",
            "Persistent recall, durable session memory",
            &["memory::store", "memory::recall", "memory::session::list"],
        ),
        (
            "browser",
            "Headless browser automation",
            &["browser::navigate", "browser::click", "browser::read_page"],
        ),
        (
            "llm-router",
            "LLM provider routing + retries",
            &[
                "agentos::llm::route",
                "agentos::llm::complete",
                "agentos::llm::providers",
            ],
        ),
        (
            "agent-core",
            "Agent lifecycle + chat orchestration",
            &["agent::chat", "agent::create", "agent::list_functions"],
        ),
        (
            "approval",
            "Permission gating for sensitive ops",
            &["approval::check", "approval::decide", "approval::list"],
        ),
        (
            "council",
            "Multi-agent governance + voting",
            &["council::submit", "council::decide", "council::proposals"],
        ),
        (
            "realm",
            "Multi-tenant agent contexts",
            &["realm::create", "realm::list"],
        ),
        (
            "evolve",
            "Function lineage + version evolution",
            &["evolve::generate", "evolve::fork", "evolve::lineage"],
        ),
        (
            "workflow",
            "YAML-defined multi-step automations",
            &["workflow::run", "workflow::list"],
        ),
        (
            "orchestrator",
            "Cross-agent task coordination",
            &[
                "orchestrator::plan",
                "orchestrator::execute",
                "orchestrator::status",
            ],
        ),
        (
            "task-decomposer",
            "Break complex tasks into subtasks",
            &["task::decompose", "task::spawn_workers", "task::list"],
        ),
        (
            "hashline",
            "Hash-anchored line edits with content-hash checks",
            &["hashline::read", "hashline::edit", "hashline::diff"],
        ),
        (
            "hooks",
            "Pre/post tool-call hooks",
            &["hook::register", "hook::fire", "hook::list"],
        ),
        (
            "vault",
            "Encrypted secret storage",
            &["vault::get", "vault::set", "vault::rotate"],
        ),
        (
            "rate-limiter",
            "Per-tenant request throttling",
            &["rate::check", "rate::get_status"],
        ),
        (
            "mcp-client",
            "Model Context Protocol bridge",
            &["mcp::connect", "mcp::list_tools", "mcp::call_tool"],
        ),
        (
            "skillkit-bridge",
            "External skill registry sync",
            &["skillkit::search", "skillkit::install", "skillkit::run"],
        ),
        (
            "hand-runner",
            "Persona-bundled function dispatch",
            &["hand::list", "hand::trigger"],
        ),
        (
            "a2a-cards",
            "Agent-to-agent capability cards",
            &["a2a::generate_card", "a2a::list_cards", "a2a::well_known"],
        ),
        (
            "a2a",
            "Agent-to-agent transport",
            &["a2a::send_task", "a2a::get_task", "a2a::handle_task"],
        ),
        (
            "bridge",
            "External runtime invocation",
            &["bridge::register", "bridge::invoke"],
        ),
        (
            "channel-slack",
            "Slack channel I/O",
            &["channel::slack::events", "channel::slack::send"],
        ),
        (
            "channel-discord",
            "Discord channel I/O",
            &["channel::discord::webhook"],
        ),
        (
            "channel-email",
            "Email send/receive",
            &["channel::email::webhook"],
        ),
        (
            "channel-bluesky",
            "Bluesky channel I/O",
            &["channel::bluesky::webhook"],
        ),
        (
            "channel-mastodon",
            "Mastodon channel I/O",
            &["channel::mastodon::webhook"],
        ),
        (
            "channel-matrix",
            "Matrix channel I/O",
            &["channel::matrix::webhook"],
        ),
        (
            "channel-reddit",
            "Reddit channel I/O",
            &["channel::reddit::webhook"],
        ),
        (
            "channel-signal",
            "Signal channel I/O",
            &["channel::signal::webhook"],
        ),
        (
            "channel-teams",
            "Teams channel I/O",
            &["channel::teams::webhook"],
        ),
        (
            "channel-telegram",
            "Telegram channel I/O",
            &["channel::telegram::webhook"],
        ),
        (
            "channel-twitch",
            "Twitch channel I/O",
            &["channel::twitch::webhook"],
        ),
        (
            "channel-webex",
            "Webex channel I/O",
            &["channel::webex::webhook"],
        ),
        (
            "channel-whatsapp",
            "WhatsApp channel I/O",
            &["channel::whatsapp::webhook"],
        ),
        (
            "channel-linkedin",
            "LinkedIn channel I/O",
            &["channel::linkedin::webhook"],
        ),
        (
            "security",
            "RBAC + taint tracking + signing",
            &[
                "security::check_capability",
                "security::scan_injection",
                "security::audit",
            ],
        ),
        (
            "wasm-sandbox",
            "Sandboxed wasm function exec",
            &["wasm::execute", "wasm::validate", "wasm::list_modules"],
        ),
        (
            "ledger",
            "Budget + spend tracking",
            &["ledger::set_budget", "ledger::spend", "ledger::summary"],
        ),
        (
            "session-replay",
            "Time-travel debugging",
            &["replay::record", "replay::search", "replay::summary"],
        ),
        (
            "session-lifecycle",
            "Session start/end hooks",
            &["lifecycle::transition", "lifecycle::get_state"],
        ),
        (
            "context-manager",
            "Context window budget control",
            &["context::budget", "context::trim", "context::build_prompt"],
        ),
        (
            "context-cache",
            "LLM response caching",
            &[
                "context_cache::get_or_fetch",
                "context_cache::invalidate",
                "context_cache::stats",
            ],
        ),
        (
            "telemetry",
            "Engine + worker observability",
            &["telemetry::summary", "telemetry::dashboard"],
        ),
        (
            "mission",
            "Long-running mission tracking",
            &["mission::create", "mission::transition", "mission::list"],
        ),
        (
            "directive",
            "Task routing directives + ancestry",
            &[
                "directive::create",
                "directive::list",
                "directive::ancestry",
            ],
        ),
        (
            "hierarchy",
            "Agent reporting structure",
            &["hierarchy::set", "hierarchy::tree", "hierarchy::chain"],
        ),
        (
            "loop-guard",
            "Detect runaway agent loops",
            &["guard::check", "guard::stats"],
        ),
        (
            "pulse",
            "Scheduled function invocation",
            &["pulse::register", "pulse::tick", "pulse::status"],
        ),
        (
            "feedback",
            "Evolved-function review + promotion",
            &["feedback::review", "feedback::improve", "feedback::promote"],
        ),
        (
            "eval",
            "Function evaluation history",
            &["eval::run", "eval::history", "eval::compare"],
        ),
        (
            "coordination",
            "Channel-based coord",
            &["coord::create_channel", "coord::post", "coord::read"],
        ),
        (
            "swarm",
            "Multi-agent swarm runs",
            &["swarm::create", "swarm::broadcast", "swarm::consensus"],
        ),
        (
            "code-agent",
            "Detect + sandbox-execute agent-written code",
            &["agent::code_detect", "agent::code_execute"],
        ),
        (
            "lsp-tools",
            "Language server primitives",
            &["lsp::diagnostics", "lsp::symbols", "lsp::goto_definition"],
        ),
        (
            "approval-tiers",
            "Tiered approval policies",
            &["approval::classify", "approval::decide_tier"],
        ),
        (
            "security-headers",
            "HTTP header policy",
            &["security::headers_apply", "security::headers_check"],
        ),
        (
            "security-map",
            "Mutual auth protocol (HMAC challenge/response)",
            &[
                "security::map_challenge",
                "security::map_respond",
                "security::map_verify",
            ],
        ),
        (
            "security-zeroize",
            "Memory zeroization",
            &["security::zeroize_wrap", "security::zeroize_check"],
        ),
        (
            "skill-security",
            "Skill permission gating",
            &[
                "skill::verify_signature",
                "skill::scan_content",
                "skill::sandbox_test",
            ],
        ),
        (
            "context-monitor",
            "Context-window watchdog",
            &["context::health", "context::compress", "context::prune"],
        ),
        (
            "cron",
            "Scheduled triggers",
            &["cron::create", "cron::list", "trigger::create"],
        ),
        (
            "streaming",
            "Chat transport — HTTP + buffered SSE framing",
            &["stream::chat", "stream::completion", "stream::sse"],
        ),
        (
            "embedding",
            "Vector embeddings (Python)",
            &["embedding::generate", "embedding::similarity"],
        ),
    ];
    ENTRIES
        .iter()
        .map(|(name, desc, fns)| WorkerCard {
            name: (*name).into(),
            description: (*desc).into(),
            functions: fns.iter().map(|s| (*s).into()).collect(),
            installed: false,
            binary_path: None,
        })
        .collect()
}

pub fn install_command(card: &WorkerCard) -> String {
    if card.installed {
        format!(
            "$ {}",
            card.binary_path
                .clone()
                .unwrap_or_else(|| format!("./target/release/{}", card.name))
        )
    } else {
        format!(
            "$ cargo build --release -p {} && ./target/release/{}",
            card.name, card.name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_card() {
        let v: Value = serde_json::from_str(
            r#"[{"name":"memory","description":"persistent recall","functions":["memory::store","memory::recall"]}]"#,
        )
        .unwrap();
        let cards = parse_catalog(&v, &[]);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].name, "memory");
        assert_eq!(cards[0].functions.len(), 2);
        assert!(!cards[0].installed);
    }

    #[test]
    fn marks_installed() {
        let v: Value = serde_json::from_str(r#"[{"name":"browser"}]"#).unwrap();
        let cards = parse_catalog(&v, &["browser".to_string()]);
        assert!(cards[0].installed);
    }

    #[test]
    fn install_cmd_for_uninstalled() {
        let card = WorkerCard {
            name: "memory".into(),
            description: "".into(),
            functions: vec![],
            installed: false,
            binary_path: None,
        };
        let cmd = install_command(&card);
        assert!(cmd.contains("cargo build"));
        assert!(cmd.contains("memory"));
    }

    #[test]
    fn install_cmd_for_installed_uses_binary() {
        let card = WorkerCard {
            name: "memory".into(),
            description: "".into(),
            functions: vec![],
            installed: true,
            binary_path: Some("/opt/memory".into()),
        };
        assert_eq!(install_command(&card), "$ /opt/memory");
    }

    /// The catalogue is hardcoded, so it drifts. Re-derive the real ids from
    /// `workers/**` and fail on any row that advertises a function nobody
    /// registers — `code-agent` used to claim `code::run`, while the worker
    /// registers `agent::code_detect` and `agent::code_execute`.
    fn workers_directory() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../workers")
    }

    fn worker_sources(directory: &std::path::Path, found: &mut Vec<std::path::PathBuf>) {
        let entries = std::fs::read_dir(directory)
            .unwrap_or_else(|error| panic!("read {}: {error}", directory.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if matches!(
                    name.as_str(),
                    "target" | "node_modules" | "__pycache__" | ".venv"
                ) {
                    continue;
                }
                worker_sources(&path, found);
            } else if matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("rs") | Some("py")
            ) {
                found.push(path);
            }
        }
    }

    /// Everything from the first `#[cfg(test)]` on is fixture code. No worker in
    /// this tree registers a function after that marker, so cutting there keeps
    /// the id set to what production actually registers.
    fn production_source(source: &str) -> &str {
        match source.find("#[cfg(test)]") {
            Some(index) => &source[..index],
            None => source,
        }
    }

    /// `const NAME: &str = "value";` / `static NAME: &str = "value";`
    fn string_constants(source: &str) -> std::collections::HashMap<String, String> {
        let mut constants = std::collections::HashMap::new();
        for line in source.lines() {
            let line = line.trim();
            let Some(rest) = line
                .strip_prefix("const ")
                .or_else(|| line.strip_prefix("static "))
            else {
                continue;
            };
            let Some((name, value)) = rest.split_once(':') else {
                continue;
            };
            if !value.contains("str") {
                continue;
            }
            let Some(literal) = value
                .split_once('"')
                .and_then(|(_, tail)| tail.split_once('"').map(|(literal, _)| literal.to_string()))
            else {
                continue;
            };
            constants.insert(name.trim().to_string(), literal);
        }
        constants
    }

    fn registered_function_ids() -> std::collections::BTreeSet<String> {
        let mut sources = Vec::new();
        worker_sources(&workers_directory(), &mut sources);
        assert!(
            !sources.is_empty(),
            "no worker sources under {}",
            workers_directory().display()
        );

        let mut ids = std::collections::BTreeSet::new();
        for path in sources {
            let text =
                std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            let text = production_source(&text);
            let constants = string_constants(text);
            for (index, _) in text.match_indices("register_function(") {
                let argument = text[index + "register_function(".len()..]
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim();
                if let Some(literal) = argument
                    .strip_prefix('"')
                    .and_then(|rest| rest.split('"').next())
                {
                    ids.insert(literal.to_string());
                } else if let Some(resolved) = constants.get(argument) {
                    ids.insert(resolved.clone());
                }
            }
        }
        ids
    }

    #[test]
    fn builtin_catalog_only_advertises_registered_functions() {
        let registered = registered_function_ids();
        assert!(
            registered.len() > 200,
            "only {} ids extracted; the extractor is broken, not the catalogue",
            registered.len()
        );
        assert!(registered.contains("agent::code_execute"));

        let mut unknown = Vec::new();
        for card in builtin_catalog() {
            for function in &card.functions {
                if !registered.contains(function) {
                    unknown.push(format!("{}: {function}", card.name));
                }
            }
        }
        assert!(
            unknown.is_empty(),
            "worker_picker advertises function ids that no worker registers: {unknown:#?}"
        );
    }

    #[test]
    fn builtin_catalog_rows_match_worker_directories() {
        let workers = workers_directory();
        let mut missing = Vec::new();
        for card in builtin_catalog() {
            if !workers.join(&card.name).is_dir() {
                missing.push(card.name.clone());
            }
            assert!(
                !card.functions.is_empty(),
                "{} advertises no functions",
                card.name
            );
        }
        assert!(
            missing.is_empty(),
            "worker_picker lists workers that do not exist: {missing:?}"
        );
    }
}
