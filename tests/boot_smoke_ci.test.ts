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
