import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";

test("boot smoke installs its pinned SDK probe runtime before execution", () => {
  const workflow = readFileSync(new URL("../.github/workflows/ci.yml", import.meta.url), "utf8");
  const job = workflow.split(/^  boot-smoke:\n/m)[1]?.split(/^  [A-Za-z_][A-Za-z0-9_-]*:\n/m)[0] ?? "";
  expect(job).toContain("oven-sh/setup-bun@0c5077e51419868618aeaa5fe8019c62421857d6");
  expect(job).toContain("bun-version: 1.3.14");
  expect(job).toContain("run: bun install --frozen-lockfile");
  expect(job.indexOf("oven-sh/setup-bun@")).toBeLessThan(job.indexOf("run: bun install --frozen-lockfile"));
  expect(job.indexOf("run: bun install --frozen-lockfile")).toBeLessThan(job.indexOf("run: bash scripts/boot-smoke.sh"));
});

test("OCI CI builds once and reuses the tested image without skipping acceptance", () => {
  const workflow = readFileSync(new URL("../.github/workflows/ci.yml", import.meta.url), "utf8");
  const [boot, smoke] = ["boot-smoke", "e2e-smoke"].map((name) =>
    workflow.split(new RegExp(`^  ${name}:\\n`, "m"))[1]?.split(/^  [A-Za-z_][A-Za-z0-9_-]*:\n/m)[0] ?? "",
  );
  for (const job of [boot, smoke]) {
    expect(job).toContain("AGENTOS_OCI_RUNTIME: docker");
    expect(job).toContain("name: agentos-oci-image");
  }
  expect(boot).toContain("run: bash scripts/boot-smoke.sh");
  expect(boot).toContain("docker image save");
  expect(boot.indexOf("run: bash scripts/boot-smoke.sh")).toBeLessThan(boot.indexOf("docker image save"));
  expect(smoke).toContain("needs: [rust, boot-smoke]");
  expect(smoke).toContain("docker image load");
  expect(smoke).toContain('python3 scripts/oci-smoke.py --report "$report" --no-build');
  expect(smoke).toContain('bun scripts/assert-oci-results.ts "$report"');
  expect(smoke.indexOf("docker image load")).toBeLessThan(smoke.indexOf("python3 scripts/oci-smoke.py"));
});

test("Node SDK gates select a runtime with native TypeScript support", () => {
  const workflow = readFileSync(new URL("../.github/workflows/ci.yml", import.meta.url), "utf8");
  for (const name of ["node-unit", "e2e-smoke", "e2e-full"]) {
    const job = workflow.split(new RegExp(`^  ${name}:\\n`, "m"))[1]?.split(/^  [A-Za-z_][A-Za-z0-9_-]*:\n/m)[0] ?? "";
    expect(job).toContain("actions/setup-node@820762786026740c76f36085b0efc47a31fe5020");
    expect(job).toContain("node-version: 24");
  }
});
