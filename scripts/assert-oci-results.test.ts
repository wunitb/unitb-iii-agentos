import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { describe, expect, it } from "vitest";

const execute = promisify(execFile);
const script = fileURLToPath(new URL("./assert-oci-results.ts", import.meta.url));
const engine = (await readFile(new URL("../.iii-version", import.meta.url), "utf8")).trim();

function receipt() {
  return {
    schema: "agentos-oci-acceptance/v1", mode: "fixture", success: true, engine,
    image: "a".repeat(64), provider_requests: 1, restart: true, data_preserved: true,
    key_preserved: true, teardown: true,
    checks: ["registry", "worker-identities", "access-control", "fake-chat", "fake-protocol", "realm", "mission", "security", "wasm"],
  };
}

async function check(value: unknown, raw = false) {
  const directory = await mkdtemp(join(tmpdir(), "agentos-oci-report-"));
  try {
    const file = join(directory, "report.json");
    await writeFile(file, raw ? String(value) : JSON.stringify(value));
    try {
      const result = await execute("bun", [script, file], { timeout: 5_000 });
      return { code: 0, output: result.stdout + result.stderr };
    } catch (error) {
      const result = error as { code?: number; stdout?: string; stderr?: string };
      return { code: result.code ?? -1, output: (result.stdout ?? "") + (result.stderr ?? "") };
    }
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

describe("OCI result gate", () => {
  it("accepts the complete fixture receipt with verified teardown", async () => {
    expect((await check(receipt())).code).toBe(0);
  });
  it("rejects missing, skipped, duplicated or inventory-only checks", async () => {
    for (const checks of [[], ["registry"], [...receipt().checks.slice(1), "registry", "registry"]]) {
      expect((await check({ ...receipt(), checks })).code).not.toBe(0);
    }
  });
  it("requires real provider execution and lifecycle evidence", async () => {
    for (const change of [{ provider_requests: 0 }, { restart: false }, { data_preserved: false }, { key_preserved: false }, { teardown: false }, { success: false }, { mode: "live" }, { engine: "0.0.0" }, { image: "mutable-tag" }]) {
      expect((await check({ ...receipt(), ...change })).code).not.toBe(0);
    }
  });
  it("rejects invalid JSON without echoing captured values", async () => {
    const secret = "private-report-canary";
    const result = await check(`{${secret}`, true);
    expect(result.code).not.toBe(0);
    expect(result.output).not.toContain(secret);
    for (const value of [null, [], {}]) expect((await check(value)).code).not.toBe(0);
  });
});
