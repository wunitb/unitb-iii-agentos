import { afterEach, describe, expect, it } from "bun:test";
import { cp, mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const repository = new URL("../", import.meta.url);
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

async function waitForQueueProvider(binary: string): Promise<string> {
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
    if (
      result.exitCode === 0 &&
      lastOutput.includes('"worker_name": "queue-engine"')
    ) {
      return lastOutput;
    }
    await Bun.sleep(100);
  }
  throw new Error(`queue provider did not register within 15s:\n${lastOutput}`);
}

describe("iii 0.22.1 boot compatibility", () => {
  liveEngineTest(
    "boots the checkout config and exposes the standalone queue provider",
    async () => {
      const binary = iii as string;
      const version = await run([binary, "--version"]);
      expect(version.exitCode).toBe(0);
      expect(version.stdout.trim()).toBe("0.22.1");

      const runtime = await mkdtemp(join(tmpdir(), "agentos-config-boot-"));
      temporaryDirectories.push(runtime);
      await cp(new URL("config.yaml", repository), join(runtime, "config.yaml"));
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
        expect(provider).toContain('"worker_name": "queue-engine"');
      } finally {
        engine.kill("SIGINT");
      }

      const exitCode = await engine.exited;
      const logs = `${await stdout}\n${await stderr}`;
      expect(exitCode).toBe(0);
      expect(logs).not.toContain("Duplicate worker configurations");
      expect(logs).not.toContain("is the deprecated name for");
      expect(logs).toContain("Function engine::queue::enqueue");
    },
    30_000,
  );
});
