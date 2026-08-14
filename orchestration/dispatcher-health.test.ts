import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { FLEET_SCHEMA_VERSION, FleetStore } from "./fleet-core";

const ROOT = resolve(import.meta.dir, "..");
const DISPATCHER = join(import.meta.dir, "dispatcher.ts");

interface HealthReport {
  ok: boolean;
  socket: boolean;
  schema?: string;
}

interface Fixture {
  configPath: string;
  runtimeDir: string;
  socketPath: string;
}

const roots: string[] = [];
const servers: Server[] = [];

afterEach(() => {
  for (const server of servers.splice(0)) server.close();
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

function fixture(): Fixture {
  const runtimeDir = join(tmpdir(), `unitb-dispatcher-health-${crypto.randomUUID()}`);
  mkdirSync(runtimeDir, { recursive: true, mode: 0o700 });
  roots.push(runtimeDir);

  const configPath = join(runtimeDir, "fleet.config.json");
  writeFileSync(configPath, JSON.stringify({
    version: 4,
    session: "unitb-dispatcher-health",
    workspaceLabel: "unitb-iii-agentos",
    repo: "wunitb/unitb-iii-agentos",
    runtimeDir,
    worktreeDir: join(runtimeDir, "worktrees"),
    maxTeams: 1,
    credentialProxy: {
      bind: "127.0.0.1:49137",
      upstreamUrl: "http://127.0.0.1:8765",
      upstreamTokenFile: "~/.omp/auth-broker.token",
    },
    network: {
      dnsForward: "1.1.1.1",
      allowedHostsByProvider: {
        anthropic: ["api.anthropic.com"],
        "openai-codex": ["chatgpt.com"],
      },
    },
    main: { model: "openai-codex/gpt-5.6-sol", credentialId: 1 },
    teams: [{ id: "TEAM-01", model: "anthropic/claude-opus-5", credentialId: 4 }],
    reviewer: {
      id: "Reviewer",
      routes: { "TEAM-01": { model: "openai-codex/gpt-5.6-terra", credentialId: 3 } },
    },
  }));

  return { configPath, runtimeDir, socketPath: join(runtimeDir, "dispatcher.sock") };
}

function seedSchema(runtimeDir: string, version: string): void {
  const store = new FleetStore(join(runtimeDir, "fleet.sqlite"), runtimeDir);
  store.setMeta("schema_version", version);
  store.close();
}

async function listen(socketPath: string): Promise<void> {
  const server = createServer();
  servers.push(server);
  const { promise, resolve: ready, reject } = Promise.withResolvers<void>();
  server.once("error", reject);
  server.listen(socketPath, () => ready());
  await promise;
}

async function runHealth(configPath: string): Promise<{ exitCode: number; report: HealthReport }> {
  const proc = Bun.spawn([process.execPath, DISPATCHER, "--config", configPath, "health"], {
    cwd: ROOT,
    stdout: "pipe",
    stderr: "pipe",
    env: process.env,
  });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  const line = stdout.trim().split("\n").at(-1) ?? "";
  if (!line.startsWith("{")) {
    throw new Error(`health printed no report (exit ${exitCode}): ${(stderr || stdout).trim()}`);
  }
  return { exitCode, report: JSON.parse(line) as HealthReport };
}

describe("dispatcher health command", () => {
  test("reports ok and exits zero when the schema matches and the socket is present", async () => {
    const { configPath, runtimeDir, socketPath } = fixture();
    seedSchema(runtimeDir, FLEET_SCHEMA_VERSION);
    await listen(socketPath);

    const { exitCode, report } = await runHealth(configPath);

    expect(report).toEqual({ ok: true, socket: true, schema: FLEET_SCHEMA_VERSION });
    expect(exitCode).toBe(0);
  });

  test("fails closed when the dispatcher socket is missing", async () => {
    const { configPath, runtimeDir } = fixture();
    seedSchema(runtimeDir, FLEET_SCHEMA_VERSION);

    const { exitCode, report } = await runHealth(configPath);

    expect(report).toEqual({ ok: false, socket: false, schema: FLEET_SCHEMA_VERSION });
    expect(exitCode).toBeGreaterThan(0);
  });

  test("fails closed when the recorded schema version is wrong", async () => {
    const { configPath, runtimeDir, socketPath } = fixture();
    seedSchema(runtimeDir, `${FLEET_SCHEMA_VERSION}-stale`);
    await listen(socketPath);

    const { exitCode, report } = await runHealth(configPath);

    expect(report).toEqual({ ok: false, socket: true, schema: `${FLEET_SCHEMA_VERSION}-stale` });
    expect(exitCode).toBeGreaterThan(0);
  });
});
