//! Shared capability vocabulary (contract I1).
//!
//! Both halves of the tool story have to agree on one matcher: the writer that
//! records what an agent may call and the reader that enforces it. This module
//! is that single definition — glob semantics, the deny-by-default families, and
//! the rule that a wildcard can never reach a deny-by-default id.

/// Function families that a wildcard must never grant (contract I1).
///
/// Each entry is a first segment: `shell::*`, `bridge::*`, and so on. Only an
/// exact-id capability entry plus an approval decision may allow one of these.
///
/// `coder` and `security` were added on .W's ruling (2026-09-02): `coder::*` is
/// the second surface of the same shell worker binary and writes host files
/// through the same jail, so `tools: ["*"]` used to be refused `shell::fs::write`
/// and allowed `coder::update`; `security::docker_exec` is root-equivalent,
/// `security::audit` can forge the audit chain, and `security::set_capabilities`
/// is the capability writer. This list gates MODEL-CHOSEN tool dispatch, not
/// worker-to-worker calls, so a worker calling `security::check_capability`
/// internally is unaffected.
pub const DENY_BY_DEFAULT_FAMILIES: [&str; 14] = [
    "shell", "bridge", "mcp", "hook", "cron", "vault", "state", "engine", "code", "harness",
    "browser", "wasm", "coder", "security",
];

/// True when `function_id` belongs to a deny-by-default family.
pub fn is_deny_by_default(function_id: &str) -> bool {
    let family = function_id.split("::").next().unwrap_or_default();
    !family.is_empty() && DENY_BY_DEFAULT_FAMILIES.contains(&family)
}

/// Namespace of capability GRANTS — entries that name a permission, not a
/// function (contract T1).
///
/// Nothing registers a `grant::*` id and nothing can be triggered under it; the
/// entry exists only so a reader can ask "does this agent hold this grant?"
/// through the same `security::check_capability` path and the same
/// [`capabilities_grant`] matcher as every tool entry. A grant is exact-id only:
/// `*`, `grant::*` and `grant::act_as::*` grant nothing, because a wildcard that
/// reached a grant would hand out cross-agent access by accident.
pub const GRANT_NAMESPACE: &str = "grant";

/// True when `id` names a grant rather than a callable.
pub fn is_grant(id: &str) -> bool {
    id.split("::").next() == Some(GRANT_NAMESPACE) && id.len() > GRANT_NAMESPACE.len()
}

/// The grant that lets an agent act on ANOTHER agent's stored content
/// (memory, sessions, lifecycle state): `grant::act_as::<target agent id>`.
///
/// One entry per target; there is deliberately no "act as anyone" spelling.
/// A worker that acts system-wide presents the operator bearer instead.
pub fn act_as_grant(target_agent_id: &str) -> String {
    format!("{GRANT_NAMESPACE}::act_as::{target_agent_id}")
}

/// Match one capability pattern against one function id.
///
/// `*` is a wildcard segment. A trailing `*` covers every remaining segment, so
/// `memory::*` matches `memory::store` and `memory::kv::get`; an interior `*`
/// covers exactly one segment, so `memory::*::get` matches `memory::kv::get`
/// only. An empty pattern or an empty function id matches nothing — prefix
/// (`starts_with`) matching is deliberately not used, because it lets
/// `file::read` grant `file::read_and_delete`.
pub fn capability_matches(pattern: &str, function_id: &str) -> bool {
    if pattern.is_empty() || function_id.is_empty() {
        return false;
    }

    let pattern_segments: Vec<&str> = pattern.split("::").collect();
    let id_segments: Vec<&str> = function_id.split("::").collect();

    for (index, segment) in pattern_segments.iter().enumerate() {
        let last = index + 1 == pattern_segments.len();
        if *segment == "*" && last {
            // A trailing wildcard needs at least one remaining segment.
            return id_segments.len() > index;
        }
        match id_segments.get(index) {
            Some(id_segment) if segment == id_segment || *segment == "*" => {}
            _ => return false,
        }
    }

    pattern_segments.len() == id_segments.len()
}

/// True when `patterns` grant `function_id`.
///
/// Deny-by-default ids and grants need an exact-id entry; no wildcard reaches
/// them.
pub fn capabilities_grant(patterns: &[String], function_id: &str) -> bool {
    if function_id.is_empty() {
        return false;
    }
    if is_deny_by_default(function_id) || is_grant(function_id) {
        return patterns.iter().any(|pattern| pattern == function_id);
    }
    patterns
        .iter()
        .any(|pattern| capability_matches(pattern, function_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_wildcard_covers_every_remaining_segment() {
        assert!(capability_matches("memory::*", "memory::store"));
        assert!(capability_matches("memory::*", "memory::kv::get"));
        assert!(capability_matches("*", "memory::store"));
        assert!(!capability_matches("memory::*", "memory"));
        assert!(!capability_matches("memory::*", "memoryx::store"));
    }

    #[test]
    fn interior_wildcard_covers_exactly_one_segment() {
        assert!(capability_matches("memory::*::get", "memory::kv::get"));
        assert!(!capability_matches("memory::*::get", "memory::get"));
        assert!(!capability_matches(
            "memory::*::get",
            "memory::kv::sub::get"
        ));
    }

    #[test]
    fn exact_ids_match_only_themselves() {
        assert!(capability_matches("file::read", "file::read"));
        assert!(!capability_matches("file::read", "file::read_and_delete"));
    }

    #[test]
    fn empty_patterns_and_ids_match_nothing() {
        assert!(!capability_matches("", "file::read"));
        assert!(!capability_matches("", ""));
        assert!(!capability_matches("*", ""));
        assert!(!capability_matches("file::read", ""));
    }

    #[test]
    fn wildcards_never_reach_deny_by_default_families() {
        for family in DENY_BY_DEFAULT_FAMILIES {
            let function_id = format!("{family}::anything");
            assert!(is_deny_by_default(&function_id), "{function_id}");
            assert!(
                !capabilities_grant(&["*".to_string()], &function_id),
                "{function_id} granted by a bare wildcard"
            );
            assert!(
                !capabilities_grant(&[format!("{family}::*")], &function_id),
                "{function_id} granted by a family wildcard"
            );
            assert!(
                capabilities_grant(std::slice::from_ref(&function_id), &function_id),
                "{function_id} not granted by its exact id"
            );
        }
    }

    #[test]
    fn grants_are_exact_only_and_never_reached_by_a_wildcard() {
        let grant = act_as_grant("agent-b");
        assert_eq!(grant, "grant::act_as::agent-b");
        assert!(is_grant(&grant));
        assert!(!is_grant("grant"), "the bare namespace names nothing");
        assert!(!is_grant("grants::act_as::x"));
        assert!(!is_grant("memory::grant::x"));
        assert!(
            !is_deny_by_default(&grant),
            "a grant is not a callable family; it must not widen the I1 deny set"
        );

        for wildcard in [
            "*",
            "grant::*",
            "grant::act_as::*",
            "grant::act_as::agent-*",
        ] {
            assert!(
                !capabilities_grant(&[wildcard.to_string()], &grant),
                "{wildcard} must not grant {grant}"
            );
        }
        assert!(!capabilities_grant(&[act_as_grant("agent-c")], &grant));
        assert!(capabilities_grant(std::slice::from_ref(&grant), &grant));
        assert!(
            !capabilities_grant(std::slice::from_ref(&grant), "grant::act_as::agent-b::x"),
            "an exact entry grants exactly one target"
        );
    }

    #[test]
    fn ordinary_ids_are_granted_by_wildcards() {
        let patterns = vec!["memory::*".to_string(), "workflow::list".to_string()];
        assert!(capabilities_grant(&patterns, "memory::store"));
        assert!(capabilities_grant(&patterns, "workflow::list"));
        assert!(!capabilities_grant(&patterns, "workflow::run"));
        assert!(!capabilities_grant(&[], "memory::store"));
        assert!(!capabilities_grant(&["".to_string()], "memory::store"));
    }
}
