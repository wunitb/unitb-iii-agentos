import { afterEach, describe, expect, it } from "bun:test";
import { cp, mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

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

async function waitForQueueProvider(
  binary: string,
): Promise<QueueProviderInfo> {
  const deadline = Date.now() + 15_000;
  let lastOutput = "";
  while (Date.now() < deadline) {
    const result = await run([
      binary,
      "trigger",
      "engine::functions::info",
      "--json",
      '{"function_id":"engine::queue::enqueue"}',
      "--timeout-ms",
      "1000",
    ]);
    lastOutput = `${result.stdout}\n${result.stderr}`;
    const provider =
      result.exitCode === 0 ? parseQueueProvider(result.stdout) : null;
    if (provider) return provider;
    await Bun.sleep(100);
  }
  throw new Error(`queue provider did not register within 15s:\n${lastOutput}`);
}

async function waitForFunction(binary: string, functionId: string): Promise<void> {
  const deadline = Date.now() + 30_000;
  let lastOutput = "";
  while (Date.now() < deadline) {
    const result = await run([
      binary,
      "trigger",
      "engine::functions::info",
      "--json",
      JSON.stringify({ function_id: functionId }),
      "--timeout-ms",
      "1000",
    ]);
    lastOutput = `${result.stdout}\n${result.stderr}`;
    if (result.exitCode === 0) return;
    await Bun.sleep(100);
  }
  throw new Error(`${functionId} did not register within 30s:\n${lastOutput}`);
}

describe(`iii ${expectedVersion} boot compatibility`, () => {
  liveEngineTest(
    "boots the checkout config with queue and jailed shell workers",
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

      const engine = Bun.spawn(
        [binary, "--no-update-check", "--config", "config.yaml"],
        {
          cwd: runtime,
          env: {
            ...Bun.env,
            III_TELEMETRY_ENABLED: "false",
          },
          stdout: "pipe",
          stderr: "pipe",
        },
      );
      const stdout = new Response(engine.stdout).text();
      const stderr = new Response(engine.stderr).text();

      try {
        const provider = await waitForQueueProvider(binary);
        expect(provider.function_id).toBe("engine::queue::enqueue");
        expect(queueProviderNames.has(provider.worker_name)).toBe(true);
        await waitForFunction(binary, "shell::list");
      } finally {
        engine.kill("SIGINT");
      }

      const exitCode = await engine.exited;
      const logs = `${await stdout}\n${await stderr}`;
      expect([0, 130]).toContain(exitCode);
      expect(logs).not.toContain("Duplicate worker configurations");
      expect(logs).not.toContain("is the deprecated name for");
      expect(logs).toContain("Function engine::queue::enqueue");
    },
    120_000,
  );
});
