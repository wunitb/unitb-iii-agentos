import { afterEach, describe, expect, it } from "bun:test";
import { cp, mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

/**
 * Boots the real engine against the checkout's config.yaml.
 *
 * This test cannot pick its own port. Verified against iii 0.22.1 on 2026-09-02:
 *
 *   - `config.yaml` accepts exactly two top-level fields. Adding any other one
 *     fails with `unknown field ..., expected `modules` or `workers``.
 *   - `iii --help` offers only `-c/--config`, `-v/--version` and
 *     `--no-update-check`. There is no port flag.
 *   - The worker listener address is the fixed literal `ws://0.0.0.0:49134`
 *     inside the binary.
 *   - Booting with `III_PORT`, `III_URL` or `III_ENGINE_URL` pointing elsewhere
 *     still binds 49134: `[ERROR] iii::workers::traits address 0.0.0.0:49134 is
 *     already in use`. Those variables address the *client* side only.
 *
 * So the engine port is global to the host. Two engines cannot coexist, and a
 * second one silently starts its external workers against the first one's bus —
 * which is how this test used to produce a red that was really somebody else's
 * engine. Since it cannot be isolated, it refuses to run rather than lie: when
 * 49134 is already taken the test is skipped with the reason in its own name.
 */

const repository = new URL("../", import.meta.url);
const expectedVersion = (
  await Bun.file(new URL(".iii-version", repository)).text()
).trim();
const iii = Bun.which("iii");

/** The engine's fixed worker-listener port. Not configurable in iii 0.22.1. */
const ENGINE_PORT = 49134;
const ENGINE_BIND_ERROR = `address 0.0.0.0:${ENGINE_PORT} is already in use`;

/** True when nothing else holds the engine port, tested the way the engine binds it. */
function enginePortIsFree(): boolean {
  try {
    const probe = Bun.listen({
      hostname: "0.0.0.0",
      port: ENGINE_PORT,
      socket: { data() {} },
    });
    probe.stop(true);
    return true;
  } catch {
    return false;
  }
}

const portFree = iii ? enginePortIsFree() : false;
const skipReason = !iii
  ? "no iii binary on PATH"
  : portFree
    ? ""
    : `port ${ENGINE_PORT} is already in use by another engine, and iii ${expectedVersion} cannot bind a different one`;

if (skipReason) {
  console.warn(`config_boot: skipping the live-engine test — ${skipReason}`);
}

const liveEngineTest = skipReason ? it.skip : it;
const testName = skipReason
  ? `boots the checkout config with queue and configuration workers [SKIPPED: ${skipReason}]`
  : "boots the checkout config with queue and configuration workers";

const temporaryDirectories: string[] = [];

afterEach(async () => {
  await Promise.all(
    temporaryDirectories.splice(0).map((path) =>
      rm(path, { recursive: true, force: true }),
    ),
  );
});

async function run(command: string[], cwd?: string) {
  const process = Bun.spawn(command, {
    cwd,
    env: {
      ...Bun.env,
      III_TELEMETRY_ENABLED: "false",
    },
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(process.stdout).text(),
    new Response(process.stderr).text(),
    process.exited,
  ]);
  return { exitCode, stdout, stderr };
}

/** Always addresses the engine explicitly, so the coupling is visible in the source. */
function triggerArgs(binary: string, functionId: string, payload: string): string[] {
  return [
    binary,
    "trigger",
    functionId,
    "--json",
    payload,
    "--address",
    "127.0.0.1",
    "--port",
    String(ENGINE_PORT),
    "--timeout-ms",
    "1000",
  ];
}

interface QueueProviderInfo {
  function_id: string;
  worker_name: string;
}

// Official iii v0.22.1 artifacts report "queue" on macOS and
// "queue-engine" on Linux.
const queueProviderNames = new Set(["queue", "queue-engine"]);

function parseQueueProvider(source: string): QueueProviderInfo | null {
  try {
    const value: unknown = JSON.parse(source);
    if (typeof value !== "object" || value === null) return null;
    const record = value as Record<string, unknown>;
    if (
      record.function_id !== "engine::queue::enqueue" ||
      typeof record.worker_name !== "string" ||
      !queueProviderNames.has(record.worker_name)
    ) {
      return null;
    }
    return {
      function_id: record.function_id,
      worker_name: record.worker_name,
    };
  } catch {
    return null;
  }
}

/**
 * Fails immediately if the engine we started could not take the port, instead of
 * querying whatever engine did and reporting its answer as ours.
 */
async function assertOurEngineOwnsThePort(logPath: string): Promise<void> {
  const log = await Bun.file(logPath).text().catch(() => "");
  if (log.includes(ENGINE_BIND_ERROR)) {
    throw new Error(
      `the engine this test started could not bind ${ENGINE_PORT}: another engine took it between the ` +
        `pre-flight check and the bind. iii ${expectedVersion} cannot use a different port, so this run ` +
        `proves nothing about the checkout config.\n${log.slice(-2000)}`,
    );
  }
}

/**
 * Waits for markers in the log of the engine THIS test started.
 *
 * Querying the bus first is not safe: a foreign engine on the same port answers
 * `engine::functions::info` immediately, so the query would succeed while our own
 * engine was still failing to bind. Our own log is the only evidence that the
 * engine under test is the one running.
 */
async function waitForOurEngine(logPath: string, markers: string[]): Promise<void> {
  const deadline = Date.now() + 30_000;
  let log = "";
  while (Date.now() < deadline) {
    log = await Bun.file(logPath).text().catch(() => "");
    if (log.includes(ENGINE_BIND_ERROR)) {
      throw new Error(
        `the engine this test started could not bind ${ENGINE_PORT}: another engine took it between the ` +
          `pre-flight check and the bind. iii ${expectedVersion} cannot use a different port, so this run ` +
          `proves nothing about the checkout config.`,
      );
    }
    if (markers.every((marker) => log.includes(marker))) return;
    await Bun.sleep(100);
  }
  const missing = markers.filter((marker) => !log.includes(marker));
  throw new Error(`engine did not register ${missing.join(", ")} within 30s:\n${log.slice(-2000)}`);
}

describe(`iii ${expectedVersion} boot compatibility`, () => {
  liveEngineTest(
    testName,
    async () => {
      const binary = iii as string;
      const version = await run([binary, "--version"]);
      expect(version.exitCode).toBe(0);
      expect(version.stdout.trim()).toBe(expectedVersion);

      const runtime = await mkdtemp(join(tmpdir(), "agentos-config-boot-"));
      temporaryDirectories.push(runtime);
      await cp(new URL("config.yaml", repository), join(runtime, "config.yaml"));
      await cp(new URL("iii.lock", repository), join(runtime, "iii.lock"));
      await cp(new URL("config", repository), join(runtime, "config"), {
        recursive: true,
      });
      await mkdir(join(runtime, "data"), { recursive: true });

      // Redirected to a file so the wait loops can read the engine's own errors
      // while it is still running, rather than only after it exits.
      const logPath = join(runtime, "engine.log");
      const engine = Bun.spawn(
        [
          "/bin/sh",
          "-c",
          `exec "$0" --no-update-check --config config.yaml > engine.log 2>&1`,
          binary,
        ],
        {
          cwd: runtime,
          env: {
            ...Bun.env,
            III_TELEMETRY_ENABLED: "false",
          },
          stdout: "ignore",
          stderr: "ignore",
        },
      );

      try {
        // `shell` is no longer booted by default (sec-perimeter, 2026-09-02), so
        // the second marker is a function the default stack really registers.
        await waitForOurEngine(logPath, [
          "Function engine::queue::enqueue",
          "Function configuration::get",
        ]);

        // Only now is the bus provably ours, so a query on it means something.
        const info = await run(
          triggerArgs(binary, "engine::functions::info", '{"function_id":"engine::queue::enqueue"}'),
        );
        expect(info.exitCode, `${info.stdout}\n${info.stderr}`).toBe(0);
        const provider = parseQueueProvider(info.stdout);
        expect(provider, `unexpected engine::functions::info payload: ${info.stdout}`).not.toBeNull();
        expect(provider!.function_id).toBe("engine::queue::enqueue");
        expect(queueProviderNames.has(provider!.worker_name)).toBe(true);
      } finally {
        engine.kill("SIGINT");
      }

      const exitCode = await engine.exited;
      const logs = await Bun.file(logPath).text();
      expect([0, 130]).toContain(exitCode);
      expect(logs).not.toContain(ENGINE_BIND_ERROR);
      expect(logs).not.toContain("Duplicate worker configurations");
      expect(logs).not.toContain("is the deprecated name for");
      expect(logs).toContain("Function engine::queue::enqueue");
    },
    120_000,
  );
});
