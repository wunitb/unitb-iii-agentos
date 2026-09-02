import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { describe, expect, it } from "vitest";

const execFileAsync = promisify(execFile);
const repoRoot = fileURLToPath(new URL("..", import.meta.url));

async function read(relative: string): Promise<string> {
  return readFile(new URL(relative, import.meta.url), "utf8");
}

/// Variable names declared by the dotenv template, in file order.
function declaredNames(template: string): string[] {
  return template
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0 && !line.startsWith("#"))
    .map((line) => line.split("=", 1)[0] ?? "")
    .filter((name) => /^[A-Za-z_][A-Za-z0-9_]*$/.test(name));
}

/// Every literal the source registers as a credential name, from the two
/// tables that decide what the running system reads:
/// `channel_secrets` in agent-core and the provider table in llm-router.
function extractQuotedUpperCase(source: string): string[] {
  return [...source.matchAll(/"([A-Z][A-Z0-9_]{2,})"/g)].map((match) => match[1] as string);
}

async function referencedElsewhere(name: string): Promise<boolean> {
  try {
    await execFileAsync(
      "git",
      ["grep", "-l", "--fixed-strings", name, "--", ".", ":!.env.example"],
      { cwd: repoRoot, encoding: "utf8" },
    );
    return true;
  } catch (error) {
    // `git grep` exits 1 with no output when nothing matches.
    if ((error as { code?: number }).code === 1) return false;
    throw error;
  }
}

describe("dotenv template contract", () => {
  it("declares no name that nothing else in the repository references", async () => {
    const names = declaredNames(await read("../.env.example"));
    const dead: string[] = [];
    for (const name of names) {
      if (!(await referencedElsewhere(name))) dead.push(name);
    }
    // scripts/dev-up.sh derives its allowlist from this file, so a name kept
    // here after its consumer disappeared silently widens what .env may carry.
    expect(dead).toEqual([]);
  }, 60_000);

  it("declares every channel secret workers/agent-core requires", async () => {
    const agentCore = await read("../workers/agent-core/src/main.rs");
    const table = agentCore.slice(
      agentCore.indexOf("fn channel_secrets("),
      agentCore.indexOf("async fn missing_channel_secrets("),
    );
    expect(table.length).toBeGreaterThan(0);

    const required = [...new Set(extractQuotedUpperCase(table))];
    expect(required).toContain("SLACK_BOT_TOKEN");
    const declared = new Set(declaredNames(await read("../.env.example")));
    expect(required.filter((name) => !declared.has(name))).toEqual([]);
  });

  it("declares every provider credential workers/llm-router resolves", async () => {
    const router = await read("../workers/llm-router/src/main.rs");
    const table = router.slice(
      router.indexOf("fn default_providers("),
      router.indexOf("struct RuntimeDefaultResolution"),
    );
    expect(table.length).toBeGreaterThan(0);

    const required = [...new Set(extractQuotedUpperCase(table))].filter((name) =>
      name.endsWith("_API_KEY"),
    );
    expect(required).toContain("ANTHROPIC_API_KEY");
    const declared = new Set(declaredNames(await read("../.env.example")));
    expect(required.filter((name) => !declared.has(name))).toEqual([]);
  });

  it("ships AGENTOS_API_KEY declared and empty so first run can generate it", async () => {
    const template = await read("../.env.example");
    expect(template).toMatch(/^AGENTOS_API_KEY=$/m);
    expect(declaredNames(template)).toContain("AGENTOS_API_KEY");
  });

  it("keeps the dev-up allowlist derived rather than hand-maintained", async () => {
    const devUp = await read("./dev-up.sh");
    // A second, hand-maintained list is how SLACK_BOT_TOKEN ended up rejected
    // while MEMWORKR_* was accepted.
    expect(devUp).not.toContain("trusted_runtime_names");
    expect(devUp).toContain('done < "$ROOT/.env.example"');
  });
});
