import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

const UNIVERSAL = new Set(["III_URL", "AGENTOS_API_KEY"]);
const PROCESS_BASELINE = new Set([
  "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TERM", "AGENTOS_HOME",
  "TMPDIR", "TMP", "TEMP", "LANG", "LANGUAGE", "SSL_CERT_FILE",
  "SSL_CERT_DIR", "NIX_SSL_CERT_FILE", "RUST_BACKTRACE", "RUST_LOG",
]);
const PROVIDERS = [
  "anthropic", "openai", "google", "groq", "together", "deepseek",
  "mistral", "fireworks", "openrouter", "ollama",
];
const PROVIDER_BASE_URLS = PROVIDERS.map((name) => `AGENTOS_${name.toUpperCase()}_BASE_URL`);
const AGENT_CORE_PRESENCE_ONLY = new Set([
  "BLUESKY_HANDLE", "BLUESKY_PASSWORD", "DISCORD_BOT_TOKEN", "DISCORD_PUBLIC_KEY",
  "LINKEDIN_CLIENT_SECRET", "LINKEDIN_TOKEN", "MASTODON_INSTANCE", "MASTODON_TOKEN",
  "MATRIX_HOMESERVER", "MATRIX_HS_TOKEN", "MATRIX_TOKEN", "REDDIT_CLIENT_ID",
  "REDDIT_REFRESH_TOKEN", "REDDIT_SECRET", "SIGNAL_API_URL", "SIGNAL_PHONE",
  "SLACK_BOT_TOKEN", "SLACK_SIGNING_SECRET", "SMTP_HOST", "SMTP_PASS", "SMTP_PORT",
  "SMTP_USER", "TEAMS_APP_ID", "TEAMS_APP_PASSWORD", "TEAMS_WEBHOOK_SECRET",
  "TELEGRAM_BOT_TOKEN", "TELEGRAM_SECRET_TOKEN", "TWITCH_BOT_USER_ID", "TWITCH_CLIENT_ID",
  "TWITCH_EVENTSUB_SECRET", "TWITCH_TOKEN", "WEBEX_TOKEN", "WEBEX_WEBHOOK_SECRET",
  "WHATSAPP_APP_SECRET", "WHATSAPP_PHONE_ID", "WHATSAPP_TOKEN", "WHATSAPP_VERIFY_TOKEN",
]);

function templateNames(source: string): Set<string> {
  return new Set(source.split("\n").filter((line) => /^[A-Za-z_][A-Za-z0-9_]*=/.test(line)).map((line) => line.split("=", 1)[0]!));
}

async function integrationEnvNames(): Promise<Set<string>> {
  const names = new Set<string>();
  const directory = new URL("../integrations/", import.meta.url);
  for (const file of await readdir(directory)) {
    if (!file.endsWith(".toml")) continue;
    const source = await readFile(new URL(file, directory), "utf8");
    const section = source.split("[integration.env]", 2)[1]?.split(/^\[/m, 1)[0] ?? "";
    for (const match of section.matchAll(/^([A-Z][A-Z0-9_]*)\s*=/gm)) names.add(match[1]!);
  }
  return names;
}

function parsePolicy(source: string): Map<string, string[]> {
  const policy = new Map<string, string[]>();
  for (const [index, raw] of source.split("\n").entries()) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) continue;
    const match = /^([a-z0-9]+(?:-[a-z0-9]+)*)=([A-Z][A-Z0-9_]*(?:,[A-Z][A-Z0-9_]*)*)$/.exec(line);
    expect(match, `malformed policy line ${index + 1}`).not.toBeNull();
    const worker = match![1]!;
    const keys = match![2]!.split(",");
    expect(policy.has(worker), `duplicate worker ${worker}`).toBe(false);
    expect(new Set(keys).size, `duplicate env key for ${worker}`).toBe(keys.length);
    policy.set(worker, keys);
  }
  return policy;
}

// All worker test modules start at their first #[cfg(test)]. Removing them before
// extraction prevents test payloads and comments from manufacturing consumers.
function productionRust(source: string): string {
  const production = source.split(/#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]/, 1)[0]!;
  let out = "";
  let block = false;
  let string = false;
  let escaped = false;
  for (let i = 0; i < production.length; i++) {
    const c = production[i]!;
    const n = production[i + 1];
    if (block) {
      if (c === "*" && n === "/") { block = false; i++; }
      else if (c === "\n") out += "\n";
      continue;
    }
    if (!string && c === "/" && n === "*") { block = true; i++; continue; }
    if (!string && c === "/" && n === "/") {
      while (i < production.length && production[i] !== "\n") i++;
      out += "\n";
      continue;
    }
    out += c;
    if (string) {
      if (escaped) escaped = false;
      else if (c === "\\") escaped = true;
      else if (c === '"') string = false;
    } else if (c === '"') string = true;
  }
  return out;
}

async function workerProduction(worker: string): Promise<string> {
  const directory = new URL(`../workers/${worker}/`, import.meta.url);
  const files = (await readdir(directory, { recursive: true }))
    .filter((name) => name.endsWith(".rs") && !name.split("/").includes("tests"));
  return (await Promise.all(files.map(async (name) => productionRust(await readFile(new URL(name, directory), "utf8"))))).join("\n");
}

function directAndConstReads(source: string): Set<string> {
  const reads = new Set<string>();
  const constants = new Map<string, string>();
  for (const match of source.matchAll(/\bconst\s+([A-Z][A-Z0-9_]*)\s*:\s*&str\s*=\s*"([A-Z][A-Z0-9_]*)"/g)) {
    constants.set(match[1]!, match[2]!);
  }
  for (const match of source.matchAll(/(?:std::)?env::var(?:_os)?\(\s*(?:"([A-Z][A-Z0-9_]*)"|([A-Z][A-Z0-9_]*))\s*\)/g)) {
    const key = match[1] ?? constants.get(match[2]!);
    if (key) reads.add(key);
  }
  // Channel credential helpers are actual value consumers. Only literal or
  // named-constant arguments to a helper that falls back to env::var(key)
  // count; arbitrary string mentions do not.
  if (/std::env::var\(key\)/.test(source)) {
    for (const match of source.matchAll(/\b(?:get_secret|startup_secret)\([^;]*?(?:"([A-Z][A-Z0-9_]*)"|([A-Z][A-Z0-9_]*))/gs)) {
      const key = match[1] ?? constants.get(match[2]!);
      if (key) reads.add(key);
    }
    for (const match of source.matchAll(/const\s+([A-Z][A-Z0-9_]*)\s*:\s*&\[&str\]\s*=\s*&\[([^\]]*)\]/gs)) {
      const array = match[1]!;
      if (!new RegExp(`for\\s+\\w+\\s+in\\s+${array}`).test(source)) continue;
      for (const value of match[2]!.matchAll(/"([A-Z][A-Z0-9_]*)"/g)) reads.add(value[1]!);
    }
  }
  return reads;
}

async function reviewedDynamicReads(worker: string, source: string): Promise<Set<string>> {
  if (worker === "mcp-client") {
    expect(source).toMatch(/std::env::var\(key\)/);
    return integrationEnvNames();
  }
  if (worker === "llm-router") {
    expect(source).toMatch(/std::env::var\(provider_base_url_env\(name\)\)/);
    // Provider credentials are selected from the compiled-in catalogue through
    // std::env::var(name); the explicit list is reviewed alongside that map.
    const keys = new Set([...PROVIDER_BASE_URLS, "AGENTOS_DEFAULT_MODEL", "AGENTOS_DEFAULT_PROVIDER"]);
    for (const key of ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GOOGLE_API_KEY", "GROQ_API_KEY", "TOGETHER_API_KEY", "DEEPSEEK_API_KEY", "MISTRAL_API_KEY", "FIREWORKS_API_KEY", "OPENROUTER_API_KEY", "CODEX_PROXY_API_KEY"]) keys.add(key);
    return keys;
  }
  return new Set();
}

describe("shipped Rust worker environment policy", () => {
  it("declares every Rust worker exactly once and no Python worker", async () => {
    const workersDir = new URL("../workers/", import.meta.url);
    const names = (await readdir(workersDir, { withFileTypes: true })).filter((entry) => entry.isDirectory()).map((entry) => entry.name).sort();
    const rust: string[] = [];
    const python: string[] = [];
    for (const name of names) {
      const manifest = await readFile(join(workersDir.pathname, name, "iii.worker.yaml"), "utf8");
      if (/^\s*kind:\s*python\s*$/m.test(manifest) || /^runtime:\s*python\s*$/m.test(manifest)) python.push(name); else rust.push(name);
    }
    const policy = parsePolicy(await readFile(new URL("../workers/env.allowlist", import.meta.url), "utf8"));
    expect([...policy.keys()].sort()).toEqual(rust);
    expect(rust).toHaveLength(62);
    expect(python).toEqual(["embedding"]);
  });

  it("gives universal values to all workers and only governed names", async () => {
    const policy = parsePolicy(await readFile(new URL("../workers/env.allowlist", import.meta.url), "utf8"));
    const declared = templateNames(await readFile(new URL("../.env.example", import.meta.url), "utf8"));
    const integrations = await integrationEnvNames();
    for (const [worker, keys] of policy) {
      expect(keys.slice(0, 2), worker).toEqual(["III_URL", "AGENTOS_API_KEY"]);
      expect(keys.filter((key) => !declared.has(key) && !integrations.has(key)), worker).toEqual([]);
    }
  });

  it("grants mcp-client every and only shipped integration-manifest secret", async () => {
    const policy = parsePolicy(await readFile(new URL("../workers/env.allowlist", import.meta.url), "utf8"));
    const manifestKeys = [...await integrationEnvNames()].sort();
    const mcpKeys = (policy.get("mcp-client") ?? []).slice(2).filter((key) => !["AGENTOS_INTEGRATIONS_DIR", "NODE_ENV"].includes(key)).sort();
    expect(mcpKeys).toEqual(manifestKeys);
  });

  it("covers production env reads without grants manufactured by tests or comments", async () => {
    const policy = parsePolicy(await readFile(new URL("../workers/env.allowlist", import.meta.url), "utf8"));
    const missing: string[] = [];
    const unjustified: string[] = [];
    for (const [worker, keys] of policy) {
      const source = await workerProduction(worker);
      const reads = directAndConstReads(source);
      for (const key of await reviewedDynamicReads(worker, source)) reads.add(key);
      const accepted = new Set(reads);
      if (worker === "agent-core") for (const key of AGENT_CORE_PRESENCE_ONLY) accepted.add(key);
      if (worker === "bridge") accepted.add("AGENTOS_ENABLE_PROCESS_BRIDGE");
      for (const key of reads) {
        if (!PROCESS_BASELINE.has(key) && !keys.includes(key)) missing.push(`${worker}:${key}`);
      }
      for (const key of keys.slice(2)) {
        if (!accepted.has(key)) unjustified.push(`${worker}:${key}`);
      }
    }
    expect(missing.sort()).toEqual([]);
    expect(unjustified.sort()).toEqual([]);
  });

  it("pins security allowlists and every dynamic provider endpoint to its consumer", async () => {
    const policy = parsePolicy(await readFile(new URL("../workers/env.allowlist", import.meta.url), "utf8"));
    expect(policy.get("hooks")).toContain("AGENTOS_HOOK_ALLOWLIST");
    expect(policy.get("cron")).toContain("AGENTOS_TRIGGER_ALLOWLIST");
    for (const key of PROVIDER_BASE_URLS) expect(policy.get("llm-router"), key).toContain(key);
    expect([...policy].filter(([, keys]) => keys.includes("AGENTOS_HOOK_ALLOWLIST")).map(([name]) => name)).toEqual(["hooks"]);
    expect([...policy].filter(([, keys]) => keys.includes("AGENTOS_TRIGGER_ALLOWLIST")).map(([name]) => name)).toEqual(["cron"]);
  });

  it("keeps reviewed cross-WP opt-ins scoped to their consumers", async () => {
    const policy = parsePolicy(await readFile(new URL("../workers/env.allowlist", import.meta.url), "utf8"));
    expect([...policy].filter(([, keys]) => keys.includes("AGENTOS_ANTHROPIC_BASE_URL")).map(([name]) => name)).toEqual(["llm-router"]);
    expect([...policy].filter(([, keys]) => keys.includes("AGENTOS_ENABLE_PROCESS_BRIDGE")).map(([name]) => name)).toEqual(["bridge"]);
  });
});
