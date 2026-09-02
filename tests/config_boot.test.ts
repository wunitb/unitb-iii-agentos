import { afterEach, describe, expect, it } from "bun:test";
import { cp, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

/**
 * Boots the real engine against the checkout's config.yaml, on ports nobody else
 * is using.
 *
 * This test used to run on the pinned default bus port, so a second engine
 * anywhere on the host made it start its external workers against THAT engine's
 * bus and then report the result as its own. The bus is configurable, but not
 * where you would look for it: there is no CLI flag, no top-level config key
 * (config.yaml accepts only `modules` and `workers`) and no environment
 * variable — `III_PORT`, `III_URL` and `III_ENGINE_URL` all leave the bind
 * alone. It is the mandatory `iii-worker-manager` worker entry, whose
 * `config` accepts `host`, `port`, `middleware_function_id`, `rbac` and
 * `handshake_timeout_ms`. Omit the entry and the engine appends it with
 * `WorkerManagerConfig::default()`, i.e. host `0.0.0.0` on port 49134.
 *
 * Verified on iii 0.22.1: with that entry set to 127.0.0.1:49611 the engine logs
 * `Engine listening on address: 127.0.0.1:49611` and binds it, while another
 * engine holds 49134.
 *
 * So this test rewrites the bus, HTTP and stream ports of its copied config to
 * free ones, waits on markers in the log of the engine IT started, and addresses
 * that engine explicitly. It shares nothing with any other engine on the host.
 */

const repository = new URL("../", import.meta.url);
const expectedVersion = (
  await Bun.file(new URL(".iii-version", repository)).text()
).trim();
const iii = Bun.which("iii");
const liveEngineTest = iii ? it : it.skip;
const temporaryDirectories: string[] = [];

afterEach(async () => {
  await Promise.all(
    temporaryDirectories.splice(0).map((path) =>
      rm(path, { recursive: true, force: true }),
    ),
  );
});

/** An OS-assigned port, released immediately. Bound the way the engine binds it. */
function freePort(): number {
  const probe = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data() {} } });
  const port = probe.port;
  probe.stop(true);
  return port;
}

/**
 * Pins the engine bus to `127.0.0.1:<port>`, replacing an existing
 * `iii-worker-manager` entry or inserting one when the checkout has none.
 */
export function pinBusAddress(config: string, port: number): string {
  const block = `  - name: iii-worker-manager\n    config:\n      host: 127.0.0.1\n      port: ${port}\n`;
  const entry = /^ *- *name: *iii-worker-manager *$/m.exec(config);
  if (!entry) {
    const workers = /^workers: *$/m.exec(config);
    if (!workers) throw new Error("config.yaml declares no workers list");
    const at = workers.index + workers[0].length + 1;
    return `${config.slice(0, at)}${block}${config.slice(at)}`;
  }
  const rest = config.slice(entry.index + entry[0].length);
  const next = /^ *- *name:/m.exec(rest);
  const tail = next ? rest.slice(next.index) : "";
  return `${config.slice(0, entry.index)}${block}${tail}`;
}

/** Rewrites the single `port:` line of a `config/<worker>.yaml` value block. */
async function repointWorkerPort(path: string, port: number): Promise<void> {
  const source = await readFile(path, "utf8");
  const updated = source.replace(/^( *port: *)\d+ *$/m, `$1${port}`);
  if (updated === source) throw new Error(`${path} has no port to repoint`);
  await writeFile(path, updated);
}

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

/** Always addresses the engine this test started, never "whatever is on the default port". */
function triggerArgs(binary: string, functionId: string, port: number): string[] {
  return [
    binary,
    "trigger",
    "engine::functions::info",
    "--json",
    JSON.stringify({ function_id: functionId }),
    "--address",
    "127.0.0.1",
    "--port",
    String(port),
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
 * Waits for markers in the log of the engine THIS test started. Querying the bus
 * as evidence of readiness is not safe on its own: any engine answers
 * `engine::functions::info`, so the query would succeed even if ours never came up.
 */
async function waitForOurEngine(logPath: string, markers: string[]): Promise<void> {
  const deadline = Date.now() + 30_000;
  let log = "";
  while (Date.now() < deadline) {
    log = await Bun.file(logPath).text().catch(() => "");
    const bindFailure = /address [^\s]+ is already in use/.exec(log);
    if (bindFailure) {
      throw new Error(
        `the engine this test started could not bind its own port (${bindFailure[0]}). ` +
          `The test picks free ports, so this means one was taken between the probe and the bind.`,
      );
    }
    if (markers.every((marker) => log.includes(marker))) return;
    await Bun.sleep(100);
  }
  const missing = markers.filter((marker) => !log.includes(marker));
  throw new Error(`engine did not reach ${missing.join(", ")} within 30s:\n${log.slice(-2000)}`);
}

/**
 * SIGINT to the engine does not take its external workers with it: `iii-worker`
 * children are reparented to init and survive, which leaks a `shell`,
 * `console`, `state`, ... process per run. The engine records each child as
 * `pid: Some(N)`, so the ones this test caused are known exactly and are the
 * only ones it touches.
 */
async function reapExternalWorkers(log: string): Promise<void> {
  const pids = [...log.matchAll(/^ *[├└] pid: Some\((\d+)\)$/gm)].map((match) => Number(match[1]));
  for (const pid of new Set(pids)) {
    try {
      process.kill(pid, "SIGTERM");
    } catch {
      // already gone
    }
  }
  if (pids.length === 0) return;
  for (let attempt = 0; attempt < 40; attempt += 1) {
    const alive = [...new Set(pids)].filter((pid) => {
      try {
        process.kill(pid, 0);
        return true;
      } catch {
        return false;
      }
    });
    if (alive.length === 0) return;
    await Bun.sleep(50);
  }
}

describe(`iii ${expectedVersion} boot compatibility`, () => {
  liveEngineTest(
    "boots the checkout config on its own ports with queue and configuration workers",
    async () => {
      const binary = iii as string;
      const version = await run([binary, "--version"]);
      expect(version.exitCode).toBe(0);
      expect(version.stdout.trim()).toBe(expectedVersion);

      const runtime = await mkdtemp(join(tmpdir(), "agentos-config-boot-"));
      temporaryDirectories.push(runtime);
      await cp(new URL("config", repository), join(runtime, "config"), {
        recursive: true,
      });
      await cp(new URL("iii.lock", repository), join(runtime, "iii.lock"));
      await mkdir(join(runtime, "data"), { recursive: true });

      const busPort = freePort();
      const checkoutConfig = await Bun.file(new URL("config.yaml", repository)).text();
      await writeFile(join(runtime, "config.yaml"), pinBusAddress(checkoutConfig, busPort));
      // The HTTP and stream servers are shared host resources too; give this
      // engine its own so a neighbour cannot make it look broken.
      await repointWorkerPort(join(runtime, "config", "iii-http.yaml"), freePort());
      await repointWorkerPort(join(runtime, "config", "iii-stream.yaml"), freePort());

      // Redirected to a file so the wait loop can read the engine's own errors
      // while it runs, rather than only after it exits.
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
          env: { ...Bun.env, III_TELEMETRY_ENABLED: "false" },
          stdout: "ignore",
          stderr: "ignore",
        },
      );

      try {
        await waitForOurEngine(logPath, [
          `Engine listening on address: 127.0.0.1:${busPort}`,
          "Function engine::queue::enqueue",
          // `shell` is no longer booted by default (sec-perimeter, 2026-09-02),
          // so this is a function the default stack really registers.
          "Function configuration::get",
        ]);

        const info = await run(triggerArgs(binary, "engine::queue::enqueue", busPort));
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
      await reapExternalWorkers(logs);
      expect([0, 130]).toContain(exitCode);
      expect(logs).not.toContain("is already in use");
      expect(logs).not.toContain("Duplicate worker configurations");
      expect(logs).not.toContain("is the deprecated name for");
      expect(logs).toContain("Function engine::queue::enqueue");
      // Proof the bus never touched the default port.
      expect(logs).toContain(`Engine listening on address: 127.0.0.1:${busPort}`);
      expect(busPort).not.toBe(49134);
    },
    120_000,
  );

  it("pins the bus whether or not the checkout already declares the worker", () => {
    const inserted = pinBusAddress("workers:\n  - name: state\n", 51000);
    expect(inserted).toBe(
      "workers:\n  - name: iii-worker-manager\n    config:\n      host: 127.0.0.1\n      port: 51000\n  - name: state\n",
    );

    const replaced = pinBusAddress(
      "workers:\n  - name: iii-worker-manager\n    config:\n      host: 0.0.0.0\n  - name: state\n",
      51001,
    );
    expect(replaced).toBe(
      "workers:\n  - name: iii-worker-manager\n    config:\n      host: 127.0.0.1\n      port: 51001\n  - name: state\n",
    );
    expect(replaced).not.toContain("0.0.0.0");

    const trailing = pinBusAddress("workers:\n  - name: iii-worker-manager\n", 51002);
    expect(trailing).toBe(
      "workers:\n  - name: iii-worker-manager\n    config:\n      host: 127.0.0.1\n      port: 51002\n",
    );
  });
});
