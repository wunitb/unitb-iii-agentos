import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, copyFileSync, writeFileSync, readFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";

const roots: string[] = [];
function fixture(args: string[], failure = "") {
  const root = mkdtempSync(join(tmpdir(), "agentos-dev-oci-"));
  roots.push(root);
  mkdirSync(join(root, "scripts"));
  copyFileSync(new URL("./dev-up.sh", import.meta.url), join(root, "scripts/dev-up.sh"));
  writeFileSync(join(root, ".env"), "not a dotenv file; must never be read\n");
  writeFileSync(join(root, ".agentos-dev.pids"), "1\n");
  const log = join(root, "commands");
  writeFileSync(join(root, "scripts/oci-stack.sh"), `#!/bin/sh
printf '%s\\n' "$*" >> "$LOG"
[ "$1" != "$FAIL" ] || exit 17
`);
  const result = spawnSync("bash", [join(root, "scripts/dev-up.sh"), ...args], {
    cwd: tmpdir(), encoding: "utf8", env: { PATH: process.env.PATH, HOME: root, LOG: log, FAIL: failure },
  });
  return { root, result, calls: existsSync(log) ? readFileSync(log, "utf8").trim().split("\n") : [] };
}
afterEach(() => { for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true }); });

describe("dev-up OCI compatibility entry point", () => {
  it("delegates default up without reading checkout dotenv or native PID files", () => {
    const { root, result, calls } = fixture([]);
    expect(result.status, result.stderr).toBe(0);
    expect(calls).toEqual(["up"]);
    expect(readFileSync(join(root, ".agentos-dev.pids"), "utf8")).toBe("1\n");
    expect(readFileSync(join(root, ".env"), "utf8")).toContain("must never be read");
  });
  it.each(["up", "stop", "status", "logs", "doctor", "build"])("forwards %s only to the OCI owner", (action) => {
    const { result, calls } = fixture([action]);
    expect(result.status).toBe(0);
    expect(calls).toEqual([action]);
  });
  it("keeps --build ordering and aborts on a failed image build", () => {
    expect(fixture(["--build"]).calls).toEqual(["build", "up"]);
    const { result, calls } = fixture(["--build"], "build");
    expect(result.status).toBe(17);
    expect(calls).toEqual(["build"]);
  });
  it("passes operator exec arguments without shell evaluation", () => {
    const { result, calls } = fixture(["exec", "printf", "%s", "a; echo not-executed"]);
    expect(result.status).toBe(0);
    expect(calls).toEqual(["exec printf %s a; echo not-executed"]);
  });
  it("retains --stop alias and rejects unsupported startup flags", () => {
    expect(fixture(["--stop"]).calls).toEqual(["stop"]);
    const bad = fixture(["--native"]);
    expect(bad.result.status).toBe(2);
    expect(bad.calls).toEqual([]);
  });
  it("is valid Bash", () => {
    expect(() => execFileSync("bash", ["-n", new URL("./dev-up.sh", import.meta.url).pathname])).not.toThrow();
  });
});
