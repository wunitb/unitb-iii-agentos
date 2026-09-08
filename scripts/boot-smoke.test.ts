import { execFileSync } from "node:child_process";
import { expect, test } from "vitest";

test("OCI smoke runs its hermetic fixture, lifecycle and failure-propagation tests", () => {
  const output = execFileSync("python3", [new URL("./boot-smoke.test.py", import.meta.url).pathname], { encoding: "utf8", timeout: 30000 });
  expect(output).toContain("OCI smoke unit checks passed");
});
