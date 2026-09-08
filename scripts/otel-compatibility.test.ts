import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { describe, expect, it } from "vitest";

const execFileAsync = promisify(execFile);
const fixture = fileURLToPath(new URL("./fixtures/otel-compatibility.mjs", import.meta.url));

describe("pinned iii SDK with maintained OpenTelemetry", () => {
  it("keeps the protocol pin and reproducible compatibility patch without vulnerable packages", async () => {
    const manifest = JSON.parse(await readFile(new URL("../package.json", import.meta.url), "utf8"));
    const version = (await readFile(new URL("../.iii-version", import.meta.url), "utf8")).trim();
    const lock = await readFile(new URL("../bun.lock", import.meta.url), "utf8");
    expect(manifest.dependencies["iii-sdk"]).toBe(version);
    expect(manifest.patchedDependencies[`@iii-dev/helpers@${version}`]).toBe(
      `patches/@iii-dev%2Fhelpers@${version}.patch`,
    );
    expect(lock).not.toMatch(/@opentelemetry\/core@1\./);
    expect(lock).not.toContain("@opentelemetry/propagator-jaeger");
    expect(lock).toContain(`@opentelemetry/core@${manifest.overrides["@opentelemetry/core"]}`);
  });

  it("keeps real engine ingestion as a required CI gate after pinned installation", async () => {
    const manifest = JSON.parse(await readFile(new URL("../package.json", import.meta.url), "utf8"));
    const ci = await readFile(new URL("../.github/workflows/ci.yml", import.meta.url), "utf8");
    expect(manifest.scripts["test:otel:native"]).toBe("node scripts/native-otel.mjs");
    const lane = ci.slice(ci.indexOf("  e2e-smoke:"), ci.indexOf("  e2e-full:"));
    const install = lane.indexOf("bash scripts/install-iii.sh");
    const gate = lane.indexOf("run: bun run test:otel:native");
    expect(install).toBeGreaterThanOrEqual(0);
    expect(gate).toBeGreaterThan(install);
    expect(gate).toBeLessThan(lane.indexOf("- name: credential-free worker integration tests"));
    expect(lane).not.toContain("continue-on-error");
  });

  it.each([
    ["node", "esm"],
    ["node", "cjs"],
    ["bun", "esm"],
    ["bun", "cjs"],
  ])("exports correlated traces, metrics and logs with %s/%s", async (runtime, format) => {
    const args = runtime === "bun" ? ["--no-env-file", fixture, format] : [fixture, format];
    const { stdout, stderr } = await execFileAsync(runtime, args, {
      env: { PATH: process.env.PATH },
      timeout: 15_000,
      maxBuffer: 1024 * 1024,
    });
    expect(stderr).toBe("");
    expect(stdout).toContain(`AGENTOS_OTEL_OK ${format}`);
  }, 20_000);
});
