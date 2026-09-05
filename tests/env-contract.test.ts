import { readdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

const root = new URL("..", import.meta.url);

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

describe("shipped Rust worker environment policy", () => {
  it("declares every Rust worker exactly once and no Python worker", async () => {
    const workersDir = new URL("../workers/", import.meta.url);
    const names = (await readdir(workersDir, { withFileTypes: true }))
      .filter((entry) => entry.isDirectory())
      .map((entry) => entry.name)
      .sort();
    const rust: string[] = [];
    const python: string[] = [];
    for (const name of names) {
      const manifest = await readFile(join(workersDir.pathname, name, "iii.worker.yaml"), "utf8");
      if (/^\s*kind:\s*python\s*$/m.test(manifest)) python.push(name);
      else if (/^runtime:\s*python\s*$/m.test(manifest)) python.push(name);
      else rust.push(name);
    }
    const policy = parsePolicy(await readFile(new URL("../workers/env.allowlist", import.meta.url), "utf8"));
    expect([...policy.keys()].sort()).toEqual(rust);
    expect(rust).toHaveLength(62);
    expect(python).toEqual(["embedding"]);
  });

  it("gives universal bus identity values to all workers and only template-declared keys", async () => {
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
    const mcpKeys = (policy.get("mcp-client") ?? []).slice(2)
      .filter((key) => !["AGENTOS_INTEGRATIONS_DIR", "NODE_ENV"].includes(key))
      .sort();
    expect(mcpKeys).toEqual(manifestKeys);
  });

  it("declares every template key each Rust worker source names", async () => {
    const policy = parsePolicy(await readFile(new URL("../workers/env.allowlist", import.meta.url), "utf8"));
    const template = templateNames(await readFile(new URL("../.env.example", import.meta.url), "utf8"));
    const missing: string[] = [];
    for (const [worker, keys] of policy) {
      const workerDir = new URL(`../workers/${worker}/`, import.meta.url);
      const files = (await readdir(workerDir, { recursive: true })).filter((name) => name.endsWith(".rs"));
      const source = (await Promise.all(files.map((name) => readFile(new URL(name, workerDir), "utf8")))).join("\n");
      for (const key of template) {
        if (source.includes(key) && !keys.includes(key)) missing.push(`${worker}:${key}`);
      }
    }
    expect(missing).toEqual([]);
  });

  it("does not grant worker-specific keys with no source consumer", async () => {
    const policy = parsePolicy(await readFile(new URL("../workers/env.allowlist", import.meta.url), "utf8"));
    const crossWp = new Set([
      "bridge:AGENTOS_ENABLE_PROCESS_BRIDGE",
      "llm-router:AGENTOS_ANTHROPIC_BASE_URL",
    ]);
    for (const key of await integrationEnvNames()) crossWp.add(`mcp-client:${key}`);
    const unjustified: string[] = [];
    for (const [worker, keys] of policy) {
      const workerDir = new URL(`../workers/${worker}/`, import.meta.url);
      const files = (await readdir(workerDir, { recursive: true }))
        .filter((name) => /(?:\.rs|Cargo\.toml)$/.test(name));
      const source = (await Promise.all(files.map((name) => readFile(new URL(name, workerDir), "utf8")))).join("\n");
      for (const key of keys.slice(2)) {
        if (!source.includes(key) && !crossWp.has(`${worker}:${key}`)) unjustified.push(`${worker}:${key}`);
      }
    }
    expect(unjustified).toEqual([]);
  });

  it("declares the two cross-WP opt-ins only for their consumers", async () => {
    const policy = parsePolicy(await readFile(new URL("../workers/env.allowlist", import.meta.url), "utf8"));
    expect(policy.get("llm-router")).toContain("AGENTOS_ANTHROPIC_BASE_URL");
    expect(policy.get("bridge")).toContain("AGENTOS_ENABLE_PROCESS_BRIDGE");
    expect([...policy].filter(([, keys]) => keys.includes("AGENTOS_ANTHROPIC_BASE_URL")).map(([name]) => name)).toEqual(["llm-router"]);
    expect([...policy].filter(([, keys]) => keys.includes("AGENTOS_ENABLE_PROCESS_BRIDGE")).map(([name]) => name)).toEqual(["bridge"]);
  });
});
