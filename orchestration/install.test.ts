import { afterEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const ROOT = resolve(import.meta.dir, "..");
const INSTALLER = join(import.meta.dir, "install.sh");
const COMPLETION_NOTICE = "UnitB fleet installation completed.";
const HEALTH_FAILURE_NOTICE = "Fleet dispatcher health check failed:";
const INVALID_HEALTH_NOTICE = "Fleet dispatcher returned an invalid health report:";
const STUBBED_COMMANDS = [
  "sudo",
  "systemctl",
  "omp",
  "bwrap",
  "pasta",
  "herdr",
  "nft",
  "socat",
  "sqlite3",
  "apparmor_parser",
];
const SUCCESS_STUB = "#!/bin/sh\nexit 0\n";

type HealthMode = "healthy" | "unhealthy" | "ok-false" | "malformed" | "command-failure";

const FAILURE_CASES: Array<{ mode: Exclude<HealthMode, "healthy">; notice: string }> = [
  { mode: "unhealthy", notice: HEALTH_FAILURE_NOTICE },
  { mode: "ok-false", notice: INVALID_HEALTH_NOTICE },
  { mode: "malformed", notice: INVALID_HEALTH_NOTICE },
  { mode: "command-failure", notice: HEALTH_FAILURE_NOTICE },
];
const roots: string[] = [];

afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

function writeExecutable(path: string, source: string): void {
  writeFileSync(path, source);
  chmodSync(path, 0o755);
}

async function runInstaller(mode: HealthMode): Promise<{ exitCode: number; stdout: string; stderr: string }> {
  const root = mkdtempSync(join(tmpdir(), "unitb-fleet-install-"));
  roots.push(root);
  const binDir = join(root, "bin");
  mkdirSync(binDir);

  for (const command of STUBBED_COMMANDS) {
    writeExecutable(join(binDir, command), SUCCESS_STUB);
  }
  writeExecutable(join(binDir, "git"), "#!/bin/sh\nprintf '%s\\n' \"$FLEET_TEST_REPO_ROOT\"\n");
  writeExecutable(join(binDir, "bun"), `#!/usr/bin/env bash
set -euo pipefail
if [[ "\${1:-}" == */orchestration/dispatcher.ts && "\${2:-}" == health ]]; then
  case "$FLEET_TEST_HEALTH_MODE" in
    healthy) printf '%s\\n' '{"ok":true,"socket":true,"schema":"3"}' ;;
    unhealthy) printf '%s\\n' '{"ok":false,"socket":false,"schema":"3"}'; exit 1 ;;
    ok-false) printf '%s\\n' '{"ok":false,"socket":true,"schema":"3"}' ;;
    malformed) printf '%s\\n' 'not-json' ;;
    command-failure) exit 7 ;;
    *) exit 64 ;;
  esac
  exit 0
fi
exec "$FLEET_TEST_REAL_BUN" "$@"
`);

  const proc = Bun.spawn(["bash", INSTALLER], {
    cwd: ROOT,
    stdout: "pipe",
    stderr: "pipe",
    env: {
      ...process.env,
      HOME: root,
      XDG_CONFIG_HOME: join(root, "config"),
      PATH: `${binDir}:${process.env.PATH ?? ""}`,
      FLEET_TEST_HEALTH_MODE: mode,
      FLEET_TEST_REAL_BUN: process.execPath,
      FLEET_TEST_REPO_ROOT: ROOT,
    },
  });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  return { exitCode, stdout, stderr };
}

describe("fleet installer health gate", () => {
  test("completes only for an explicit healthy report", async () => {
    const { exitCode, stdout } = await runInstaller("healthy");

    expect(exitCode).toBe(0);
    expect(stdout).toContain(COMPLETION_NOTICE);
  });

  for (const { mode, notice } of FAILURE_CASES) {
    test(`fails closed for ${mode}`, async () => {
      const { exitCode, stdout, stderr } = await runInstaller(mode);

      expect(exitCode).toBeGreaterThan(0);
      expect(stdout).not.toContain(COMPLETION_NOTICE);
      expect(stderr).toContain(notice);
    });
  }
});
