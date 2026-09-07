import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { describe, expect, it } from "vitest";

const execFileAsync = promisify(execFile);
const script = fileURLToPath(new URL("./assert-e2e-results.ts", import.meta.url));
const title = "agent::chat — local fake Anthropic provider";
const fullName = `AgentOS full-stack E2E ${title}`;

function report(assertions: unknown[] = [{ title, fullName, status: "passed" }]) {
  return {
    success: true,
    numFailedTests: 0,
    numFailedTestSuites: 0,
    testResults: [{ name: "/fixture/e2e/full-stack.test.ts", assertionResults: assertions }],
  };
}

async function check(value: unknown, raw = false) {
  const directory = await mkdtemp(join(tmpdir(), "agentos-e2e-result-test-"));
  const file = join(directory, "result.json");
  try {
    await writeFile(file, raw ? String(value) : JSON.stringify(value));
    try {
      const result = await execFileAsync("bun", [script, file], { timeout: 5_000 });
      return { code: 0, output: result.stdout + result.stderr };
    } catch (error) {
      const result = error as { code?: number; stdout?: string; stderr?: string };
      return { code: result.code ?? -1, output: (result.stdout ?? "") + (result.stderr ?? "") };
    }
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

describe("fake-provider result gate", () => {
  it("accepts one real chat test that passed in the expected file", async () => {
    const result = await check(report());
    expect(result.code).toBe(0);
    expect(result.output).toContain("actual fake-provider chat test passed");
  });

  it("rejects skipped, pending, todo and failed real chat tests", async () => {
    for (const status of ["skipped", "pending", "todo", "failed"]) {
      expect((await check(report([{ title, fullName, status }]))).code).not.toBe(0);
    }
  });

  it("does not confuse the always-on preflight with the actual chat test", async () => {
    const result = await check(report([
      { title: "local fake Anthropic provider configuration preflight", status: "passed" },
      { title, fullName, status: "skipped" },
    ]));
    expect(result.code).not.toBe(0);
  });

  it("rejects no result, wrong file, and duplicate titles", async () => {
    expect((await check(report([]))).code).not.toBe(0);
    const wrongFile = report();
    wrongFile.testResults[0].name = "/fixture/e2e/another.test.ts";
    expect((await check(wrongFile)).code).not.toBe(0);
    expect((await check(report([
      { title, fullName, status: "passed" }, { title, fullName, status: "passed" },
    ]))).code).not.toBe(0);
  });

  it("refuses a passing leaf inside an unsuccessful run", async () => {
    for (const override of [
      { success: false }, { numFailedTests: 1 }, { numFailedTestSuites: 1 },
    ]) {
      expect((await check({ ...report(), ...override })).code).not.toBe(0);
    }
  });

  it("requires the exact suite and leaf fullName, not just the leaf title", async () => {
    for (const wrongName of [undefined, title, `Unrelated suite ${title}`]) {
      const result = await check(report([{ title, fullName: wrongName, status: "passed" }]));
      expect(result.code).not.toBe(0);
    }
  });

  it("CI produces JSON and applies the actual-chat gate to that same report", async () => {
    const ci = await readFile(new URL("../.github/workflows/ci.yml", import.meta.url), "utf8");
    const start = ci.indexOf("      - name: credential-free worker integration tests");
    expect(start).toBeGreaterThan(-1);
    const step = ci.slice(start, ci.indexOf("\n      - name:", start + 1));
    expect(step).toContain('--reporter=json');
    expect(step).toContain('--outputFile="$report"');
    expect(step).toContain('bun scripts/assert-e2e-results.ts "$report"');
    expect(step.indexOf('bun scripts/assert-e2e-results.ts')).toBeGreaterThan(step.indexOf('bunx vitest'));
  });

  it("fails closed on invalid report structure without echoing report contents", async () => {
    const canary = "sensitive-report-canary-do-not-echo";
    for (const value of [null, [], {}, { ...report(), testResults: null }]) {
      expect((await check(value)).code).not.toBe(0);
    }
    const result = await check(`{${canary}`, true);
    expect(result.code).not.toBe(0);
    expect(result.output).not.toContain(canary);
  });
});
