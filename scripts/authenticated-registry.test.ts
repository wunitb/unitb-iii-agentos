import { execFile } from "node:child_process";
import { copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { afterEach, describe, expect, it } from "vitest";

import { readGeneratedApiKey } from "./authenticated-registry.js";

const generatedKey = "0123456789abcdef".repeat(4);
const execFileAsync = promisify(execFile);
const fixtureRoots: string[] = [];

afterEach(async () => {
  await Promise.all(fixtureRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

async function runHelper(fail: boolean) {
  const root = await mkdtemp(join(tmpdir(), "agentos-authenticated-registry-test-"));
  fixtureRoots.push(root);
  const sdk = join(root, "node_modules", "iii-sdk");
  await mkdir(sdk, { recursive: true });
  await copyFile(new URL("./authenticated-registry.ts", import.meta.url), join(root, "helper.ts"));
  await writeFile(
    join(sdk, "package.json"),
    JSON.stringify({ name: "iii-sdk", version: "0.22.1", type: "module", exports: "./index.js" }),
  );
  await writeFile(
    join(sdk, "index.js"),
    `export function registerWorker(_url, options) {
  const bearer = options.headers.Authorization;
  console.log("sdk-connect " + bearer);
  console.error("sdk-stderr " + bearer);
  return {
    async trigger() {
      console.info("sdk-trigger " + bearer);
      if (process.env.SDK_STUB_FAIL === "1") throw new Error("sdk-secret " + bearer);
      return { functions: [{ function_id: "agent::chat", worker_name: "agentos-agent-core" }] };
    },
    async shutdown() { console.warn("sdk-shutdown " + bearer); },
  };
}
`,
  );
  const dotenv = join(root, "runtime.env");
  const output = join(root, "registry.json");
  await writeFile(dotenv, `AGENTOS_API_KEY=${generatedKey}\n`);
  try {
    const result = await execFileAsync(
      "bun",
      ["--no-env-file", join(root, "helper.ts"), dotenv, output],
      { env: { ...process.env, SDK_STUB_FAIL: fail ? "1" : "0", III_URL: "ws://fixture" } },
    );
    return { ...result, exitCode: 0, output };
  } catch (error) {
    const failure = error as { code?: number; stdout?: string; stderr?: string };
    return {
      exitCode: failure.code ?? -1,
      stdout: failure.stdout ?? "",
      stderr: failure.stderr ?? "",
      output,
    };
  }
}

describe("authenticated registry helper", () => {
  it("reads the one generated API key without accepting unrelated values", () => {
    expect(
      readGeneratedApiKey(
        `# generated runtime\nAUDIT_HMAC_KEY=${"f".repeat(64)}\nAGENTOS_API_KEY=${generatedKey}\n`,
      ),
    ).toBe(generatedKey);
  });

  it.each([
    ["missing", "AUDIT_HMAC_KEY=x\n"],
    ["empty", "AGENTOS_API_KEY=\n"],
    ["duplicate", `AGENTOS_API_KEY=${generatedKey}\nAGENTOS_API_KEY=${generatedKey}\n`],
    ["not generated hex", "AGENTOS_API_KEY=operator-value\n"],
  ])("rejects the %s generated-key contract", (_name, source) => {
    expect(() => readGeneratedApiKey(source)).toThrow(/generated AGENTOS_API_KEY/);
  });

  it("writes parseable JSON while suppressing SDK stdout and stderr", async () => {
    const result = await runHelper(false);
    expect(result.exitCode).toBe(0);
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("");
    expect(result.stdout + result.stderr).not.toContain(generatedKey);
    expect(JSON.parse(await readFile(result.output, "utf8"))).toEqual({
      functions: [{ function_id: "agent::chat", worker_name: "agentos-agent-core" }],
    });
  });

  it("keeps SDK error details and credentials out of failed subprocess output", async () => {
    const result = await runHelper(true);
    expect(result.exitCode).not.toBe(0);
    expect(result.stdout).toBe("");
    expect(result.stderr).toBe("authenticated registry query failed\n");
    expect(result.stdout + result.stderr).not.toContain(generatedKey);
    await expect(readFile(result.output, "utf8")).rejects.toThrow();
  });

  it("uses the pinned SDK with a bearer header and never reads a parent key", async () => {
    const source = await readFile(new URL("./authenticated-registry.ts", import.meta.url), "utf8");
    expect(source).toContain('await import("iii-sdk")');
    expect(source).toContain("Authorization: `Bearer ${apiKey}`");
    expect(source).not.toContain("process.env.AGENTOS_API_KEY");
    expect(source).not.toContain("console.log(apiKey)");
  });
});
