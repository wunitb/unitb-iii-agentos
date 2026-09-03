import { describe, expect, it } from "bun:test";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { collectHttpRoutes, collectRegistrations, withoutComments, withoutTestModules } from "../scripts/counts";

/**
 * Contract T1 (tenancy): a `memory::*` or `vault::*` handler resolves WHO a
 * call is from — a `principal` set by a trusted worker, or the operator bearer
 * in `headers` — and never from the payload's `agentId`. A call that carries
 * neither fails closed. So every worker that dispatches one of those ids has
 * to label the call, and this scanner is what stops the next caller from
 * forgetting: it finds each dispatch of a literal `memory::…` / `vault::…` id
 * under `workers/` and resolves its payload expression — a `json!` literal, a
 * builder function in the same file, or an `attach_agent` call — to prove the
 * label is there.
 *
 * `crates/http-adapter/src/principal.rs` is the one definition of the label;
 * `workers/memory`, `workers/vault` and `workers/session-lifecycle` are the
 * readers. Dispatches through a variable id (`step.function_id`,
 * `func_id.to_string()`) are invisible to a static scan; those sites
 * (workflow, wasm-sandbox) are covered by their own unit tests instead.
 */

interface KnownUnlabelledDispatch {
  /** `<file> <function id>` */
  readonly key: string;
  /** ISO date the exception was recorded. */
  readonly since: string;
  /** Where the fix belongs. */
  readonly owner: string;
  readonly reason: string;
}

/**
 * Dispatches that predate contract T1 and were ALREADY refused by the vault
 * before it: `workers/vault` has required the operator bearer on every
 * `vault::get` since PR #60, so each of these calls has answered
 * `Unauthorized` and the worker has fallen back to its environment variable
 * ever since. Nothing changed for them in the tenancy PR. The fix is one line
 * per worker — `"headers": agentos_bus_auth::handshake_headers()`, as
 * `workers/agent-core::vault_read_payload` does — and is recorded as a request
 * for the owning WP. The suite below fails if an entry outlives its dispatch,
 * so this list can only ever shrink.
 */
const CHANNEL_WORKERS_READING_THE_VAULT_WITHOUT_A_BEARER = [
  "bluesky", "discord", "email", "linkedin", "mastodon", "matrix", "reddit",
  "signal", "slack", "teams", "telegram", "twitch", "webex", "whatsapp",
] as const;

const KNOWN_UNLABELLED_DISPATCHES: KnownUnlabelledDispatch[] =
  CHANNEL_WORKERS_READING_THE_VAULT_WITHOUT_A_BEARER.map((channel) => ({
    key: `workers/channel-${channel}/src/main.rs vault::get`,
    since: "2026-09-03",
    owner: `workers/channel-${channel}/src/main.rs`,
    reason:
      "get_secret sends {key} with no bearer; the vault has refused it since PR #60 and the worker " +
      "falls back to the environment variable. Fix: add \"headers\": agentos_bus_auth::handshake_headers().",
  }));

const repositoryRoot = new URL("../", import.meta.url).pathname;
const SKIP_DIRECTORIES = new Set([".git", ".worktrees", "node_modules", "target"]);

function workerRustSources(): string[] {
  const found: string[] = [];
  const walk = (relative: string): void => {
    for (const entry of readdirSync(join(repositoryRoot, relative))) {
      if (SKIP_DIRECTORIES.has(entry)) continue;
      const path = join(relative, entry);
      if (statSync(join(repositoryRoot, path)).isDirectory()) walk(path);
      else if (entry.endsWith(".rs")) found.push(path);
    }
  };
  walk("workers");
  return found.sort();
}

function lineOf(source: string, index: number): number {
  return source.slice(0, index).split("\n").length;
}

/**
 * One call argument starting at `start`: balanced over `()`, `[]` and `{}`,
 * ending before the top-level `,` or the closing `)` of the enclosing call.
 */
function argumentAt(text: string, start: number): string {
  let depth = 0;
  let index = start;
  while (index < text.length) {
    const character = text[index];
    if (character === "(" || character === "[" || character === "{") depth += 1;
    else if (character === ")" || character === "]" || character === "}") {
      if (depth === 0) break;
      depth -= 1;
    } else if (character === "," && depth === 0) break;
    index += 1;
  }
  return text.slice(start, index);
}

/** The brace-balanced body of `fn <name>(` in `text`, if the file defines it. */
function functionBody(text: string, name: string): string | undefined {
  const match = new RegExp(`\\bfn\\s+${name}\\s*[(<]`).exec(text);
  if (!match) return undefined;
  const open = text.indexOf("{", match.index);
  if (open < 0) return undefined;
  let depth = 0;
  for (let index = open; index < text.length; index += 1) {
    const character = text[index];
    if (character === "{") depth += 1;
    else if (character === "}") {
      depth -= 1;
      if (depth === 0) return text.slice(open, index + 1);
    }
  }
  return undefined;
}

const LABEL = /"principal"\s*:|"headers"\s*:|principal::attach_agent\(|principal::as_agent\(|handshake_headers\(/;

/**
 * True when the payload expression provably carries a principal or a bearer:
 * directly, or through a builder function defined in the same file.
 */
export function payloadCarriesPrincipal(expression: string, fileText: string, seen = new Set<string>()): boolean {
  if (LABEL.test(expression)) return true;
  const builder = /^\s*([A-Za-z_][A-Za-z0-9_]*)\s*\(/.exec(expression);
  if (!builder) return false;
  const name = builder[1]!;
  if (seen.has(name)) return false;
  seen.add(name);
  const body = functionBody(fileText, name);
  return body !== undefined && payloadCarriesPrincipal(body, fileText, seen);
}

const ID_LITERAL = /"(memory|vault)::[A-Za-z0-9_:]+"/g;
/** The literal is the second argument of a call whose first argument is the bus. */
const SECOND_ARGUMENT = /\(\s*&?[A-Za-z_][A-Za-z0-9_.]*\s*,\s*$/;
const TRIGGER_FIELD = /function_id\s*:\s*$/;
const REGISTRATION = /register_(function|cron_trigger|http_trigger)\s*\(\s*(&?[A-Za-z_][A-Za-z0-9_]*\s*,\s*)?$/;

interface Dispatch {
  readonly file: string;
  readonly line: number;
  readonly id: string;
  readonly payload: string;
  readonly labelled: boolean;
}

export function dispatchesIn(file: string, text: string): Dispatch[] {
  const found: Dispatch[] = [];
  for (const match of text.matchAll(ID_LITERAL)) {
    const before = text.slice(Math.max(0, match.index - 120), match.index);
    if (REGISTRATION.test(before)) continue;
    const id = match[0].slice(1, -1);
    let payload: string | undefined;

    if (TRIGGER_FIELD.test(before)) {
      // `TriggerRequest { function_id: "memory::store".to_string(), payload: <expr>, ... }`
      const window = text.slice(match.index, match.index + 900);
      const field = /\bpayload\s*:/.exec(window);
      if (field) payload = argumentAt(text, match.index + field.index + field[0].length);
    } else if (SECOND_ARGUMENT.test(before)) {
      // `helper(iii, "memory::store", <expr>)`
      const after = text.slice(match.index + match[0].length);
      const separator = /^(?:\.(?:to_string|into|to_owned)\(\))?\s*,\s*/.exec(after);
      if (separator) payload = argumentAt(text, match.index + match[0].length + separator[0].length);
    }

    if (payload === undefined) continue;
    found.push({
      file,
      line: lineOf(text, match.index),
      id,
      payload: payload.trim(),
      labelled: payloadCarriesPrincipal(payload, text),
    });
  }
  return found;
}

const sources = workerRustSources().map((file) => ({
  file,
  text: withoutComments(withoutTestModules(readFileSync(join(repositoryRoot, file), "utf8"))),
}));

const dispatches = sources.flatMap(({ file, text }) => dispatchesIn(file, text));

function offenders(): Map<string, Dispatch[]> {
  const byKey = new Map<string, Dispatch[]>();
  for (const dispatch of dispatches) {
    if (dispatch.labelled) continue;
    const key = `${dispatch.file} ${dispatch.id}`;
    byKey.set(key, [...(byKey.get(key) ?? []), dispatch]);
  }
  return byKey;
}

describe("contract T1: every memory::*/vault::* dispatch carries a principal", () => {
  it("labels every dispatch outside the dated allowlist", () => {
    const allowed = new Set(KNOWN_UNLABELLED_DISPATCHES.map((entry) => entry.key));
    const unexpected = [...offenders()]
      .filter(([key]) => !allowed.has(key))
      .flatMap(([, sites]) =>
        sites.map((site) => `${site.file}:${site.line} ${site.id} payload has no principal/bearer: ${site.payload.slice(0, 80)}`),
      );
    expect(unexpected).toEqual([]);
  });

  it("keeps the allowlist sound and lets it only shrink", () => {
    const today = new Date().toISOString().slice(0, 10);
    const current = offenders();
    const seen = new Set<string>();
    for (const entry of KNOWN_UNLABELLED_DISPATCHES) {
      expect(/^\d{4}-\d{2}-\d{2}$/.test(entry.since), `${entry.key}: since must be an ISO date`).toBe(true);
      expect(entry.since <= today, `${entry.key}: since is in the future`).toBe(true);
      expect(entry.reason.length, `${entry.key}: needs a reason`).toBeGreaterThan(40);
      expect(entry.owner.length, `${entry.key}: needs an owning file`).toBeGreaterThan(0);
      expect(seen.has(entry.key), `${entry.key}: listed twice`).toBe(false);
      seen.add(entry.key);
      expect(current.has(entry.key), `${entry.key}: the dispatch is labelled now — delete this entry`).toBe(true);
    }
  });

  it("sees the labelled dispatches, so an empty offender list means clean and not unscanned", () => {
    const labelled = dispatches.filter((dispatch) => dispatch.labelled).map((dispatch) => `${dispatch.file} ${dispatch.id}`);
    for (const expected of [
      "workers/agent-core/src/main.rs memory::recall",
      "workers/agent-core/src/main.rs memory::store",
      "workers/agent-core/src/main.rs vault::get",
      "workers/swarm/src/main.rs memory::store",
      "workers/security-map/src/main.rs vault::get",
    ]) {
      expect(labelled, `${expected} should be found and labelled`).toContain(expected);
    }
    expect(sources.length).toBeGreaterThan(50);
  });

  it("never registers or routes a grant: grants are permissions, not callables", () => {
    const registered = collectRegistrations().filter((site) => site.id.startsWith("grant::"));
    expect(registered).toEqual([]);
    const routed = collectHttpRoutes().filter((site) => site.route.includes("grant::"));
    expect(routed).toEqual([]);
    // Workers never spell a grant id by hand; `policy::act_as_grant` is the one
    // place that mints it, so its shape can change without a drift.
    const spelled = sources
      .filter(({ text }) => /"grant::/.test(text))
      .map(({ file }) => file);
    expect(spelled).toEqual([]);
  });
});

describe("principal scanner", () => {
  const scan = (text: string) => dispatchesIn("probe.rs", withoutComments(withoutTestModules(text)));

  it("flags a TriggerRequest whose json! payload has no principal", () => {
    const text = 'iii.trigger(TriggerRequest { function_id: "memory::recall".to_string(), payload: json!({ "agentId": id, "query": q }), action: None, timeout_ms: None })';
    expect(scan(text).map((d) => d.labelled)).toEqual([false]);
  });

  it("accepts a json! payload with a principal or a bearer", () => {
    const principal = 'iii.trigger(TriggerRequest { function_id: "memory::recall".to_string(), payload: json!({ "principal": principal::as_agent(a), "query": q }), action: None, timeout_ms: None })';
    const bearer = 'iii.trigger(TriggerRequest { function_id: "vault::get".to_string(), payload: json!({ "key": k, "headers": agentos_bus_auth::handshake_headers() }), action: None, timeout_ms: None })';
    expect(scan(principal).map((d) => d.labelled)).toEqual([true]);
    expect(scan(bearer).map((d) => d.labelled)).toEqual([true]);
  });

  it("resolves a builder function defined in the same file, and only one level of it", () => {
    const good = 'fn build(a: &str) -> Value { json!({ "principal": principal::as_agent(a) }) }\nfire(iii, "memory::store", build(a));';
    const bad = 'fn build(a: &str) -> Value { json!({ "agentId": a }) }\nfire(iii, "memory::store", build(a));';
    const missing = 'fire(iii, "memory::store", elsewhere(a));';
    const attached = 'fire(iii, "memory::store", principal::attach_agent("memory::store", args, a));';
    expect(scan(good).map((d) => d.labelled)).toEqual([true]);
    expect(scan(bad).map((d) => d.labelled)).toEqual([false]);
    expect(scan(missing).map((d) => d.labelled)).toEqual([false]);
    expect(scan(attached).map((d) => d.labelled)).toEqual([true]);
  });

  it("does not mistake registrations, route tables or an id passed as a plain argument for dispatches", () => {
    const text = [
      'iii.register_function("memory::store", RegisterFunction::new_async(|i| async move { store(i).await }));',
      'for (id, method, path) in [("memory::kv::get", "GET", "api/memory/:key")] { register(id) }',
      'register_cron_trigger(&iii, "memory::evict".to_string(), "0 3 * * *")?;',
      'refuse_agent_maintenance(&input, "memory::evict")?;',
      'const ALLOWED: &[&str] = &["memory::recall", "memory::store"];',
    ].join("\n");
    expect(scan(text)).toEqual([]);
  });

  it("ignores dispatches inside comments and test modules", () => {
    const text = [
      '// iii.trigger(TriggerRequest { function_id: "memory::recall".to_string(), payload: json!({}), action: None, timeout_ms: None })',
      "#[cfg(test)]",
      "mod tests {",
      '    fn t() { fire(iii, "memory::store", json!({ "agentId": "a" })); }',
      "}",
    ].join("\n");
    expect(scan(text)).toEqual([]);
  });
});
