use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerCard {
    pub name: String,
    pub description: String,
    pub functions: Vec<String>,
    pub installed: bool,
    pub binary_path: Option<String>,
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
    use syn::parse::Parser;
    use syn::visit::{self, Visit};

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

    fn cfg_possibilities_without_test(meta: &syn::Meta) -> (bool, bool) {
        match meta {
            syn::Meta::Path(path) if path.is_ident("test") => (false, true),
            syn::Meta::Path(_) | syn::Meta::NameValue(_) => (true, true),
            syn::Meta::List(list) => {
                let Ok(children) =
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
                        .parse2(list.tokens.clone())
                else {
                    return (true, true);
                };
                let possibilities: Vec<_> = children
                    .iter()
                    .map(cfg_possibilities_without_test)
                    .collect();
                if list.path.is_ident("all") {
                    (
                        possibilities.iter().all(|(can_be_true, _)| *can_be_true),
                        possibilities.iter().any(|(_, can_be_false)| *can_be_false),
                    )
                } else if list.path.is_ident("any") {
                    (
                        possibilities.iter().any(|(can_be_true, _)| *can_be_true),
                        possibilities.iter().all(|(_, can_be_false)| *can_be_false),
                    )
                } else if list.path.is_ident("not") && possibilities.len() == 1 {
                    (possibilities[0].1, possibilities[0].0)
                } else {
                    (true, true)
                }
            }
        }
    }

    /// A cfg item is test-only only when its predicate cannot be true with
    /// `test = false`. Unknown feature/platform predicates are treated as
    /// potentially production so the catalogue guard fails open only on syntax
    /// it understands, not on a guessed build configuration.
    fn is_test_only(attributes: &[syn::Attribute]) -> bool {
        attributes.iter().any(|attribute| {
            if attribute
                .path()
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "test")
            {
                return true;
            }
            let syn::Meta::List(list) = &attribute.meta else {
                return false;
            };
            if !list.path.is_ident("cfg") {
                return false;
            }
            let Ok(predicates) =
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
                    .parse2(list.tokens.clone())
            else {
                return false;
            };
            predicates
                .iter()
                .any(|predicate| !cfg_possibilities_without_test(predicate).0)
        })
    }

    fn literal_id(expression: &syn::Expr) -> Option<String> {
        match expression {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(value),
                ..
            }) => Some(value.value()),
            syn::Expr::Group(group) => literal_id(&group.expr),
            syn::Expr::Paren(paren) => literal_id(&paren.expr),
            syn::Expr::Reference(reference) => literal_id(&reference.expr),
            _ => None,
        }
    }

    fn returned_factory_id(block: &syn::Block) -> Option<String> {
        if block.stmts.len() != 1 {
            return None;
        }
        let syn::Stmt::Expr(expression, _) = block.stmts.first()? else {
            return None;
        };
        let expression = match expression {
            syn::Expr::Return(returned) => returned.expr.as_deref()?,
            expression => expression,
        };
        let syn::Expr::Tuple(tuple) = expression else {
            return None;
        };
        if tuple.elems.len() != 2 || !matches!(tuple.elems.get(1), Some(syn::Expr::Path(_))) {
            return None;
        }
        literal_id(tuple.elems.first()?)
    }

    fn record_unique_definition(
        definitions: &mut std::collections::HashMap<String, Option<String>>,
        name: String,
        candidate: Option<String>,
    ) {
        match definitions.entry(name) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(candidate);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.insert(None);
            }
        }
    }

    #[derive(Default)]
    struct Definitions {
        constants: std::collections::HashMap<String, Option<String>>,
        factories: std::collections::HashMap<String, Option<String>>,
    }

    impl<'ast> Visit<'ast> for Definitions {
        fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
            if is_test_only(&function.attrs) {
                return;
            }
            let candidate = function
                .sig
                .inputs
                .is_empty()
                .then(|| returned_factory_id(&function.block))
                .flatten();
            record_unique_definition(
                &mut self.factories,
                function.sig.ident.to_string(),
                candidate,
            );
            visit::visit_item_fn(self, function);
        }

        fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
            if !is_test_only(&module.attrs) {
                visit::visit_item_mod(self, module);
            }
        }

        fn visit_item_impl(&mut self, implementation: &'ast syn::ItemImpl) {
            if !is_test_only(&implementation.attrs) {
                visit::visit_item_impl(self, implementation);
            }
        }

        fn visit_item_const(&mut self, constant: &'ast syn::ItemConst) {
            if !is_test_only(&constant.attrs) {
                record_unique_definition(
                    &mut self.constants,
                    constant.ident.to_string(),
                    literal_id(&constant.expr),
                );
            }
        }

        fn visit_item_static(&mut self, value: &'ast syn::ItemStatic) {
            if !is_test_only(&value.attrs) {
                record_unique_definition(
                    &mut self.constants,
                    value.ident.to_string(),
                    literal_id(&value.expr),
                );
            }
        }
    }

    struct Registrations<'a> {
        definitions: &'a Definitions,
        scopes: Vec<std::collections::HashMap<String, Option<String>>>,
        ids: std::collections::BTreeSet<String>,
    }

    fn pattern_bindings(pattern: &syn::Pat, bindings: &mut Vec<(String, bool)>) {
        match pattern {
            syn::Pat::Ident(identifier) => bindings.push((
                identifier.ident.to_string(),
                identifier.mutability.is_some(),
            )),
            syn::Pat::Type(typed) => pattern_bindings(&typed.pat, bindings),
            syn::Pat::Tuple(tuple) => {
                for element in &tuple.elems {
                    pattern_bindings(element, bindings);
                }
            }
            syn::Pat::Reference(reference) => pattern_bindings(&reference.pat, bindings),
            syn::Pat::Slice(slice) => {
                for element in &slice.elems {
                    pattern_bindings(element, bindings);
                }
            }
            syn::Pat::Struct(value) => {
                for field in &value.fields {
                    pattern_bindings(&field.pat, bindings);
                }
            }
            syn::Pat::TupleStruct(value) => {
                for element in &value.elems {
                    pattern_bindings(element, bindings);
                }
            }
            _ => {}
        }
    }

    fn unknown_pattern_scope(
        pattern: &syn::Pat,
    ) -> std::collections::HashMap<String, Option<String>> {
        let mut bindings = Vec::new();
        pattern_bindings(pattern, &mut bindings);
        bindings.into_iter().map(|(name, _)| (name, None)).collect()
    }

    fn condition_pattern_bindings(expression: &syn::Expr, bindings: &mut Vec<(String, bool)>) {
        match expression {
            syn::Expr::Let(value) => pattern_bindings(&value.pat, bindings),
            syn::Expr::Binary(binary) => {
                condition_pattern_bindings(&binary.left, bindings);
                condition_pattern_bindings(&binary.right, bindings);
            }
            syn::Expr::Group(group) => condition_pattern_bindings(&group.expr, bindings),
            syn::Expr::Paren(paren) => condition_pattern_bindings(&paren.expr, bindings),
            _ => {}
        }
    }

    fn immutable_identifier(pattern: &syn::Pat) -> Option<String> {
        match pattern {
            syn::Pat::Ident(identifier) if identifier.mutability.is_none() => {
                Some(identifier.ident.to_string())
            }
            syn::Pat::Type(typed) => immutable_identifier(&typed.pat),
            _ => None,
        }
    }

    fn tuple_element_zero_identifier(pattern: &syn::Pat) -> Option<String> {
        let pattern = match pattern {
            syn::Pat::Type(typed) => typed.pat.as_ref(),
            pattern => pattern,
        };
        let syn::Pat::Tuple(tuple) = pattern else {
            return None;
        };
        immutable_identifier(tuple.elems.first()?)
    }

    impl Registrations<'_> {
        /// `Some(None)` means the nearest lexical binding is known to exist but
        /// its value is unknown. Callers must stop there rather than falling
        /// through to an outer local, constant, or factory with the same name.
        fn lexical_value(&self, identifier: &str) -> Option<Option<String>> {
            for scope in self.scopes.iter().rev() {
                if let Some(value) = scope.get(identifier) {
                    return Some(value.clone());
                }
            }
            None
        }

        fn resolve_expression(&self, expression: &syn::Expr) -> Option<String> {
            if let Some(id) = literal_id(expression) {
                return Some(id);
            }
            match expression {
                syn::Expr::Path(path) => {
                    let identifier = path.path.get_ident()?.to_string();
                    match self.lexical_value(&identifier) {
                        Some(value) => value,
                        None => self
                            .definitions
                            .constants
                            .get(&identifier)
                            .cloned()
                            .flatten(),
                    }
                }
                syn::Expr::Group(group) => self.resolve_expression(&group.expr),
                syn::Expr::Paren(paren) => self.resolve_expression(&paren.expr),
                syn::Expr::Reference(reference) => self.resolve_expression(&reference.expr),
                _ => None,
            }
        }

        fn resolve_factory_call(&self, expression: &syn::Expr) -> Option<String> {
            let call = match expression {
                syn::Expr::Call(call) => call,
                syn::Expr::Group(group) => return self.resolve_factory_call(&group.expr),
                syn::Expr::Paren(paren) => return self.resolve_factory_call(&paren.expr),
                _ => return None,
            };
            let syn::Expr::Path(function) = call.func.as_ref() else {
                return None;
            };
            let identifier = function.path.get_ident()?.to_string();
            if self.lexical_value(&identifier).is_some() {
                return None;
            }
            self.definitions
                .factories
                .get(&identifier)
                .cloned()
                .flatten()
        }

        fn remember_local(&mut self, local: &syn::Local) {
            let resolved = local.init.as_ref().and_then(|initializer| {
                if let Some(name) = tuple_element_zero_identifier(&local.pat) {
                    self.resolve_factory_call(&initializer.expr)
                        .map(|id| (name, id))
                } else {
                    immutable_identifier(&local.pat).zip(self.resolve_expression(&initializer.expr))
                }
            });
            let mut bindings = Vec::new();
            pattern_bindings(&local.pat, &mut bindings);
            let Some(scope) = self.scopes.last_mut() else {
                return;
            };
            for (name, _) in &bindings {
                scope.insert(name.clone(), None);
            }
            if let Some((name, id)) = resolved {
                scope.insert(name, Some(id));
            }
        }

        fn capture_registration(
            &mut self,
            arguments: &syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>,
        ) {
            if let Some(id) = arguments
                .first()
                .and_then(|argument| self.resolve_expression(argument))
            {
                self.ids.insert(id);
            }
        }
    }

    impl Registrations<'_> {
        fn visit_function_body<'ast>(
            &mut self,
            inputs: &'ast syn::punctuated::Punctuated<syn::FnArg, syn::Token![,]>,
            block: &'ast syn::Block,
        ) {
            let outer_scopes = std::mem::take(&mut self.scopes);
            let mut parameters = std::collections::HashMap::new();
            for input in inputs {
                if let syn::FnArg::Typed(argument) = input {
                    let mut bindings = Vec::new();
                    pattern_bindings(&argument.pat, &mut bindings);
                    for (name, _) in bindings {
                        parameters.insert(name, None);
                    }
                }
            }
            self.scopes.push(parameters);
            self.visit_block(block);
            self.scopes = outer_scopes;
        }
    }

    impl<'ast> Visit<'ast> for Registrations<'_> {
        fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
            if !is_test_only(&function.attrs) {
                self.visit_function_body(&function.sig.inputs, &function.block);
            }
        }

        fn visit_impl_item_fn(&mut self, function: &'ast syn::ImplItemFn) {
            if !is_test_only(&function.attrs) {
                self.visit_function_body(&function.sig.inputs, &function.block);
            }
        }

        fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
            if !is_test_only(&module.attrs) {
                visit::visit_item_mod(self, module);
            }
        }

        fn visit_item_impl(&mut self, implementation: &'ast syn::ItemImpl) {
            if !is_test_only(&implementation.attrs) {
                visit::visit_item_impl(self, implementation);
            }
        }

        fn visit_block(&mut self, block: &'ast syn::Block) {
            self.scopes.push(std::collections::HashMap::new());
            for statement in &block.stmts {
                self.visit_stmt(statement);
            }
            self.scopes.pop();
        }

        fn visit_local(&mut self, local: &'ast syn::Local) {
            if is_test_only(&local.attrs) {
                return;
            }
            if let Some(initializer) = &local.init {
                self.visit_expr(&initializer.expr);
                if let Some((_, diverge)) = &initializer.diverge {
                    self.visit_expr(diverge);
                }
            }
            self.remember_local(local);
        }

        fn visit_expr_block(&mut self, block: &'ast syn::ExprBlock) {
            if !is_test_only(&block.attrs) {
                visit::visit_expr_block(self, block);
            }
        }

        fn visit_expr_closure(&mut self, closure: &'ast syn::ExprClosure) {
            if is_test_only(&closure.attrs) {
                return;
            }
            let mut parameters = std::collections::HashMap::new();
            for input in &closure.inputs {
                let mut bindings = Vec::new();
                pattern_bindings(input, &mut bindings);
                for (name, _) in bindings {
                    parameters.insert(name, None);
                }
            }
            self.scopes.push(parameters);
            self.visit_expr(&closure.body);
            self.scopes.pop();
        }

        fn visit_expr_for_loop(&mut self, loop_expression: &'ast syn::ExprForLoop) {
            if is_test_only(&loop_expression.attrs) {
                return;
            }
            self.visit_expr(&loop_expression.expr);
            self.scopes
                .push(unknown_pattern_scope(&loop_expression.pat));
            self.visit_block(&loop_expression.body);
            self.scopes.pop();
        }

        fn visit_expr_if(&mut self, if_expression: &'ast syn::ExprIf) {
            if is_test_only(&if_expression.attrs) {
                return;
            }
            self.visit_expr(&if_expression.cond);
            let mut bindings = Vec::new();
            condition_pattern_bindings(&if_expression.cond, &mut bindings);
            self.scopes
                .push(bindings.into_iter().map(|(name, _)| (name, None)).collect());
            self.visit_block(&if_expression.then_branch);
            self.scopes.pop();
            if let Some((_, else_expression)) = &if_expression.else_branch {
                self.visit_expr(else_expression);
            }
        }

        fn visit_expr_while(&mut self, while_expression: &'ast syn::ExprWhile) {
            if is_test_only(&while_expression.attrs) {
                return;
            }
            self.visit_expr(&while_expression.cond);
            let mut bindings = Vec::new();
            condition_pattern_bindings(&while_expression.cond, &mut bindings);
            self.scopes
                .push(bindings.into_iter().map(|(name, _)| (name, None)).collect());
            self.visit_block(&while_expression.body);
            self.scopes.pop();
        }

        fn visit_expr_match(&mut self, match_expression: &'ast syn::ExprMatch) {
            if is_test_only(&match_expression.attrs) {
                return;
            }
            self.visit_expr(&match_expression.expr);
            for arm in &match_expression.arms {
                if is_test_only(&arm.attrs) {
                    continue;
                }
                self.scopes.push(unknown_pattern_scope(&arm.pat));
                if let Some((_, guard)) = &arm.guard {
                    self.visit_expr(guard);
                }
                self.visit_expr(&arm.body);
                self.scopes.pop();
            }
        }

        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            if is_test_only(&call.attrs) {
                return;
            }
            if call.method == "register_function" {
                self.capture_registration(&call.args);
            }
            visit::visit_expr_method_call(self, call);
        }

        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if is_test_only(&call.attrs) {
                return;
            }
            if let syn::Expr::Path(function) = call.func.as_ref()
                && function
                    .path
                    .segments
                    .last()
                    .is_some_and(|segment| segment.ident == "register_function")
            {
                self.capture_registration(&call.args);
            }
            visit::visit_expr_call(self, call);
        }
    }

    fn registered_function_ids_in_source(
        source: &str,
    ) -> Result<std::collections::BTreeSet<String>, syn::Error> {
        let file = syn::parse_file(source)?;
        let mut definitions = Definitions::default();
        definitions.visit_file(&file);
        let mut registrations = Registrations {
            definitions: &definitions,
            scopes: Vec::new(),
            ids: std::collections::BTreeSet::new(),
        };
        registrations.visit_file(&file);
        Ok(registrations.ids)
    }

    /// Python workers use direct string ids. This parser is intentionally kept
    /// separate from the Rust AST extractor rather than pretending Python is
    /// Rust or applying Rust cfg rules to it.
    fn python_registered_function_ids(source: &str) -> std::collections::BTreeSet<String> {
        let mut ids = std::collections::BTreeSet::new();
        for (index, _) in source.match_indices("register_function(") {
            let argument = source[index + "register_function(".len()..]
                .split(',')
                .next()
                .unwrap_or("")
                .trim();
            if let Some(literal) = argument
                .strip_prefix(['\'', '"'])
                .and_then(|rest| rest.split(['\'', '"']).next())
            {
                ids.insert(literal.to_string());
            }
        }
        ids
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
            match path.extension().and_then(|extension| extension.to_str()) {
                Some("rs") => ids.extend(
                    registered_function_ids_in_source(&text)
                        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display())),
                ),
                Some("py") => ids.extend(python_registered_function_ids(&text)),
                _ => {}
            }
        }
        ids
    }

    #[test]
    fn extractor_keeps_production_registrations_after_standalone_test_item() {
        let source = r#"
            #[cfg(test)]
            async fn install_test_job() {
                iii.register_function("fake::helper", fake_handler);
            }

            fn main() {
                iii.register_function("pulse::register", production_handler);
                iii.register_function("pulse::tick", production_handler);
                iii.register_function("pulse::status", production_handler);
            }
        "#;

        let ids = registered_function_ids_in_source(source).expect("parse fixture");
        assert_eq!(
            ids,
            std::collections::BTreeSet::from([
                "pulse::register".to_string(),
                "pulse::status".to_string(),
                "pulse::tick".to_string(),
            ])
        );
    }

    #[test]
    fn extractor_omits_registrations_inside_test_only_module() {
        let source = r#"
            fn install(iii: &Iii) {
                iii.register_function("real::function", production_handler);
            }

            #[cfg(test)]
            mod tests {
                fn fake(iii: &Iii) {
                    iii.register_function("fake::test-only", fake_handler);
                }
            }
        "#;

        let ids = registered_function_ids_in_source(source).expect("parse fixture");
        assert_eq!(
            ids,
            std::collections::BTreeSet::from(["real::function".to_string()])
        );
    }

    #[test]
    fn extractor_resolves_literal_id_from_pure_binding_factory() {
        let source = r#"
            type Handler = fn();
            fn reject_direct_connect() {}

            fn mcp_connect_binding() -> (&'static str, Handler) {
                ("mcp::connect", reject_direct_connect)
            }

            fn main() {
                let (mcp_connect_id, mcp_connect_handler) = mcp_connect_binding();
                iii.register_function(mcp_connect_id, mcp_connect_handler);
            }
        "#;

        let ids = registered_function_ids_in_source(source).expect("parse fixture");
        assert_eq!(
            ids,
            std::collections::BTreeSet::from(["mcp::connect".to_string()])
        );
    }

    #[test]
    fn extractor_stops_at_unknown_shadow_and_refuses_mutable_inference() {
        let source = r#"
            const ID: &str = "fake::constant";

            fn same_scope(runtime_id: fn() -> &'static str) {
                let id = "fake::same-scope";
                let id = runtime_id();
                iii.register_function(id, handler);
            }

            fn inner_scope(runtime_id: fn() -> &'static str) {
                let id = ID;
                {
                    let id = runtime_id();
                    iii.register_function(id, handler);
                }
            }

            fn mutable(runtime_id: fn() -> &'static str) {
                let mut id = "fake::mutable";
                id = runtime_id();
                iii.register_function(id, handler);
            }
        "#;

        let ids = registered_function_ids_in_source(source).expect("parse fixture");
        assert!(ids.is_empty(), "unknown bindings leaked old ids: {ids:?}");
    }

    #[test]
    fn extractor_omits_test_attributes_and_cfg_test_expressions() {
        let source = r#"
            #[test]
            fn unit_fixture() {
                iii.register_function("fake::test", handler);
            }

            #[tokio::test]
            async fn async_fixture() {
                iii.register_function("fake::tokio-test", handler);
            }

            fn production() {
                #[cfg(test)]
                let _fake = iii.register_function("fake::local", handler);
                #[cfg(test)]
                {
                    iii.register_function("fake::block", handler);
                }
                iii.register_function("real::production", handler);
            }
        "#;

        let ids = registered_function_ids_in_source(source).expect("parse fixture");
        assert_eq!(
            ids,
            std::collections::BTreeSet::from(["real::production".to_string()])
        );
    }

    #[test]
    fn extractor_rejects_factory_with_another_return_path() {
        let source = r#"
            fn binding() -> (&'static str, Handler) {
                if changed() {
                    return ("other::id", other_handler);
                }
                ("fake::advertised", advertised_handler)
            }

            fn install() {
                let (id, handler) = binding();
                iii.register_function(id, handler);
            }
        "#;

        let ids = registered_function_ids_in_source(source).expect("parse fixture");
        assert!(
            ids.is_empty(),
            "dynamic factory was inferred as pure: {ids:?}"
        );
    }

    #[test]
    fn extractor_does_not_resolve_shadowed_factory_names() {
        let source = r#"
            fn binding() -> (&'static str, Handler) {
                ("fake::global", global_handler)
            }

            fn from_parameter(binding: Factory) {
                let (id, handler) = binding();
                iii.register_function(id, handler);
            }

            fn from_local(runtime_factory: Factory) {
                let binding = runtime_factory;
                let (id, handler) = binding();
                iii.register_function(id, handler);
            }
        "#;

        let ids = registered_function_ids_in_source(source).expect("parse fixture");
        assert!(ids.is_empty(), "shadowed factory leaked global id: {ids:?}");
    }

    #[test]
    fn extractor_rejects_ambiguous_same_name_factories() {
        let source = r#"
            mod one {
                fn binding() -> (&'static str, Handler) {
                    ("fake::one", one_handler)
                }
            }
            mod two {
                fn binding() -> (&'static str, Handler) {
                    ("fake::two", two_handler)
                }
            }

            fn install() {
                let (id, handler) = binding();
                iii.register_function(id, handler);
            }
        "#;

        let ids = registered_function_ids_in_source(source).expect("parse fixture");
        assert!(ids.is_empty(), "ambiguous factory was guessed: {ids:?}");
    }

    #[test]
    fn extractor_tombstones_control_flow_pattern_bindings() {
        let source = r#"
            fn install(runtime_ids: RuntimeIds, runtime: Runtime) {
                let id = "fake::outer";
                for id in runtime_ids {
                    iii.register_function(id, handler);
                }
                if let Some(id) = runtime.next_id() {
                    iii.register_function(id, handler);
                }
                while let Some(id) = runtime.next_id() {
                    iii.register_function(id, handler);
                }
                match runtime.result() {
                    Ok(id) => iii.register_function(id, handler),
                    Err(_) => {}
                }
            }
        "#;

        let ids = registered_function_ids_in_source(source).expect("parse fixture");
        assert!(
            ids.is_empty(),
            "control-flow pattern reused an outer ID: {ids:?}"
        );
    }

    #[test]
    fn extractor_maps_factory_id_only_to_tuple_element_zero_identifier() {
        let source = r#"
            const OTHER_ID: &str = "runtime::other";
            fn binding() -> (&'static str, &'static str) {
                ("fake::first", OTHER_ID)
            }

            fn destructured() {
                let (_, id) = binding();
                iii.register_function(id, handler);
            }

            fn scalar() {
                let pair = binding();
                iii.register_function(pair, handler);
            }
        "#;

        let ids = registered_function_ids_in_source(source).expect("parse fixture");
        assert!(
            ids.is_empty(),
            "factory tuple was assigned to the wrong pattern: {ids:?}"
        );
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
