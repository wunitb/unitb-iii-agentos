import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { access, chmod, copyFile, mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { promisify } from "node:util";
import { afterEach, describe, expect, it } from "vitest";

const execFileAsync = promisify(execFile);
const fixtureRoots: string[] = [];

interface CapturedEnvironment {
  codex: string | null;
  anthropic: string | null;
  openai: string | null;
  defaultModel: string | null;
}

interface MemworkrOptions {
  /// 40-character commit written to `current`.
  version?: string;
  /// Digest recorded beside the binary; "correct" records the real sha256.
  digest?: "correct" | "wrong" | "missing";
  /// Whether the fake memworkr reports a healthy schema-v6 memory::health.
  healthy?: boolean;
}

interface FixtureOptions {
  dotenvMode?: number;
  dotenvSymlink?: boolean;
  memworkr?: MemworkrOptions;
  /// Extra `integrations/<name>.toml` manifests, by integration id.
  integrations?: Record<string, string[]>;
  /// "present": ship a fake daemon; "armed-missing": arm config.yaml with no
  /// daemon binary at all.
  busAuth?: "present" | "armed-missing";
}

interface BusAuthCapture {
  argv: string[];
  /// How many times the fake daemon was executed.
  starts: number;
}

interface FixtureResult {
  captured: CapturedEnvironment | null;
  stderr: string;
  stdout: string;
  exitCode: number;
  /// Whether the fake memworkr binary was executed.
  memworkrStarted: boolean;
  /// What the fake bus-auth daemon recorded, when one was installed.
  busAuth: BusAuthCapture | null;
}

async function readCapturedValue(capturePath: string, name: string): Promise<string | null> {
  try {
    return await readFile(join(capturePath, name), "utf8");
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return null;
    throw error;
  }
}

async function runDevUpFixture(
  dotenv: string,
  inherited: NodeJS.ProcessEnv = {},
  options: FixtureOptions = {},
): Promise<FixtureResult> {
  const root = await mkdtemp(join(tmpdir(), "agentos-dev-up-"));
  fixtureRoots.push(root);

  const scriptPath = join(root, "scripts", "dev-up.sh");
  const workerPath = join(root, "target", "release", "agentos-env-probe");
  const capturePath = join(root, "captured");
  const envPath = join(root, ".env");
  const dotenvMode = options.dotenvMode ?? 0o600;

  await mkdir(dirname(scriptPath), { recursive: true });
  await mkdir(dirname(workerPath), { recursive: true });
  await copyFile(new URL("./dev-up.sh", import.meta.url), scriptPath);
  await copyFile(new URL("../.env.example", import.meta.url), join(root, ".env.example"));
  if (options.dotenvSymlink) {
    await writeFile(join(root, ".env.target"), dotenv, { mode: 0o600 });
    await symlink(".env.target", envPath);
  } else {
    await writeFile(envPath, dotenv, { mode: dotenvMode });
    await chmod(envPath, dotenvMode);
  }
  const busAuthLog = join(root, "bus-auth.argv");
  if (options.busAuth === "present") {
    const daemonPath = join(root, "target", "release", "agentos-bus-authd");
    // Holds the port the way the real daemon does, so the readiness probe in
    // dev-up.sh has something to connect to.
    await writeFile(
      daemonPath,
      `#!/usr/bin/env bash\nprintf '%s\\n' "$*" >> ${JSON.stringify(busAuthLog)}\n` +
        `port="\${1##*:}"\nexec python3 -c "import socket,time\n` +
        `s=socket.socket();s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)\n` +
        `s.bind(('127.0.0.1',int('$port')));s.listen();time.sleep(30)"\n`,
    );
    await chmod(daemonPath, 0o755);
  }
  if (options.busAuth === "armed-missing") {
    await writeFile(
      join(root, "config.yaml"),
      "workers:\n  - name: iii-worker-manager\n    config:\n      rbac:\n        auth_function_id: agentos::bus_auth\n",
    );
  }

  const memworkrMarker = join(root, "memworkr.started");
  const stubDir = join(root, "stub");
  if (options.memworkr) {
    const version = options.memworkr.version ?? "0".repeat(39) + "1";
    const versionDir = join(root, ".agentos-runtime", "memworkr", "versions", version);
    const memworkrPath = join(versionDir, "memworkr");
    await mkdir(versionDir, { recursive: true });
    await mkdir(stubDir, { recursive: true });
    await writeFile(join(root, ".agentos-runtime", "memworkr", "current"), `${version}\n`);
    await writeFile(
      memworkrPath,
      `#!/usr/bin/env bash\nprintf '%s' started > ${JSON.stringify(memworkrMarker)}\nexec sleep 30\n`,
    );
    await chmod(memworkrPath, 0o755);

    const digest = options.memworkr.digest ?? "correct";
    if (digest !== "missing") {
      const recorded =
        digest === "correct"
          ? createHash("sha256").update(await readFile(memworkrPath)).digest("hex")
          : "0".repeat(64);
      await writeFile(join(versionDir, "SHA256"), `${recorded}\n`);
    }

    // The readiness probe shells out to the real `iii` and `jq`; only `iii` is
    // faked, so the jq filter in dev-up.sh is exercised for real.
    const health = options.memworkr.healthy ?? true
      ? '{"status":"ok","schemaVersion":6,"callerEnforced":true,"instanceClaimed":true}'
      : '{"status":"degraded","schemaVersion":6}';
    const iiiStub = join(stubDir, "iii");
    await writeFile(
      iiiStub,
      `#!/usr/bin/env bash\nif [[ "\${1:-}" == "trigger" ]]; then printf '%s\\n' ${JSON.stringify(health)}; exit 0; fi\nexit 1\n`,
    );
    await chmod(iiiStub, 0o755);
  }

  for (const [id, keys] of Object.entries(options.integrations ?? {})) {
    await mkdir(join(root, "integrations"), { recursive: true });
    const declarations = keys
      .map((key) => `${key} = { required = true, description = "test" }`)
      .join("\n");
    await writeFile(
      join(root, "integrations", `${id}.toml`),
      `[integration]\nid = "${id}"\n\n[integration.env]\n${declarations}\n`,
    );
  }

  await writeFile(
    workerPath,
    `#!/usr/bin/env bash
set -euo pipefail
capture_path=${JSON.stringify(capturePath)}
capture_tmp="\${capture_path}.tmp"
mkdir "$capture_tmp"
if [[ \${CODEX_PROXY_API_KEY+x} ]]; then printf '%s' "$CODEX_PROXY_API_KEY" > "$capture_tmp/codex"; fi
if [[ \${ANTHROPIC_API_KEY+x} ]]; then printf '%s' "$ANTHROPIC_API_KEY" > "$capture_tmp/anthropic"; fi
if [[ \${OPENAI_API_KEY+x} ]]; then printf '%s' "$OPENAI_API_KEY" > "$capture_tmp/openai"; fi
if [[ \${AGENTOS_DEFAULT_MODEL+x} ]]; then printf '%s' "$AGENTOS_DEFAULT_MODEL" > "$capture_tmp/defaultModel"; fi
mv "$capture_tmp" "$capture_path"
`,
  );
  await chmod(workerPath, 0o755);

  let stderr = "";
  let stdout = "";
  let exitCode = 0;
  try {
    const result = await execFileAsync("bash", [scriptPath], {
      encoding: "utf8",
      env: {
        PATH: options.memworkr ? `${stubDir}:${process.env.PATH}` : process.env.PATH,
        HOME: process.env.HOME,
        TMPDIR: process.env.TMPDIR,
        ...inherited,
      },
    });
    stderr = result.stderr;
    stdout = result.stdout;
  } catch (error) {
    const commandError = error as {
      code?: number | string | null;
      stderr?: string;
      stdout?: string;
    };
    if (typeof commandError.code !== "number") throw error;
    exitCode = commandError.code;
    stderr = commandError.stderr ?? "";
    stdout = commandError.stdout ?? "";
  }
  const memworkrStarted = await readCapturedValue(root, "memworkr.started").then(
    (value) => value !== null,
  );
  const busAuthRaw = await readCapturedValue(root, "bus-auth.argv");
  const busAuthLines = (busAuthRaw ?? "").split("\n").filter(Boolean);
  const busAuth: BusAuthCapture | null = busAuthRaw === null
    ? null
    : { argv: busAuthLines[0]?.split(" ") ?? [], starts: busAuthLines.length };
  if (exitCode !== 0) {
    return { captured: null, stderr, stdout, exitCode, memworkrStarted, busAuth };
  }

  for (let attempt = 0; attempt < 200; attempt += 1) {
    try {
      await access(capturePath);
      break;
    } catch {
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
  }
  await access(capturePath);
  const [codex, anthropic, openai, defaultModel] = await Promise.all([
    readCapturedValue(capturePath, "codex"),
    readCapturedValue(capturePath, "anthropic"),
    readCapturedValue(capturePath, "openai"),
    readCapturedValue(capturePath, "defaultModel"),
  ]);
  return {
    captured: { codex, anthropic, openai, defaultModel },
    stderr,
    stdout,
    exitCode,
    memworkrStarted,
    busAuth,
  };
}

async function stopFixtureProcesses(root: string): Promise<void> {
  try {
    const pids = await readFile(join(root, ".agentos-dev.pids"), "utf8");
    for (const pid of pids.split("\n").filter(Boolean)) {
      try {
        process.kill(Number(pid), "SIGTERM");
      } catch {
        // Already gone: dev-up.sh degrades by killing memworkr itself.
      }
    }
  } catch {
    // No PID file: the run failed before spawning anything.
  }
}

afterEach(async () => {
  const roots = fixtureRoots.splice(0);
  await Promise.all(roots.map(stopFixtureProcesses));
  await Promise.all(roots.map((root) => rm(root, { recursive: true, force: true })));
});

describe("dev-up dotenv loading", () => {
  it("normalizes surrounding whitespace and matching quotes", async () => {
    const { captured, exitCode } = await runDevUpFixture(
      `CODEX_PROXY_API_KEY="quoted-secret"
ANTHROPIC_API_KEY=  cloud-secret${"  "}
OPENAI_API_KEY='open secret'
`,
    );
    expect(exitCode).toBe(0);

    expect(captured).toEqual({
      codex: "quoted-secret",
      anthropic: "cloud-secret",
      openai: "open secret",
      defaultModel: null,
    });
  });

  it("preserves inherited credentials when file values normalize to empty", async () => {
    const { captured, exitCode } = await runDevUpFixture(
      "CODEX_PROXY_API_KEY=   \nANTHROPIC_API_KEY=\t\n",
      {
        CODEX_PROXY_API_KEY: "inherited-codex",
        ANTHROPIC_API_KEY: "inherited-anthropic",
      },
    );
    expect(exitCode).toBe(0);

    expect(captured).toEqual({
      codex: "inherited-codex",
      anthropic: "inherited-anthropic",
      openai: null,
      defaultModel: null,
    });
  });

  it("keeps shell syntax, unmatched quotes, and inline comments inert", async () => {
    const { captured, exitCode } = await runDevUpFixture(
      `CODEX_PROXY_API_KEY=$(printf exploited)
ANTHROPIC_API_KEY=$HOME
OPENAI_API_KEY="unmatched
AGENTOS_DEFAULT_MODEL=value # literal
`,
    );
    expect(exitCode).toBe(0);

    expect(captured).toEqual({
      codex: "$(printf exploited)",
      anthropic: "$HOME",
      openai: '"unmatched',
      defaultModel: "value # literal",
    });
  });

  it("rejects a dotenv file with mode 0644 before starting workers", async () => {
    const { exitCode, stderr } = await runDevUpFixture("", {}, { dotenvMode: 0o644 });

    expect(exitCode).toBe(1);
    expect(stderr).toContain("error:");
    expect(stderr).toContain(".env must be owned by the current user and have mode 600");
  });

  it("rejects a symlink dotenv file before starting workers", async () => {
    const { exitCode, stderr } = await runDevUpFixture("", {}, { dotenvSymlink: true });

    expect(exitCode).toBe(1);
    expect(stderr).toContain("error:");
    expect(stderr).toContain(".env must be a regular file owned by the current user with mode 600");
  });

  it("rejects an unknown dotenv name with its source line number", async () => {
    const { exitCode, stderr } = await runDevUpFixture("UNKNOWN_DOTENV_NAME=\n");

    expect(exitCode).toBe(1);
    expect(stderr).toContain("error: unknown dotenv variable 'UNKNOWN_DOTENV_NAME' on line 1");
  });

  it("warns when only the default model is configured without a Codex key", async () => {
    const { exitCode, stderr } = await runDevUpFixture(
      `CODEX_PROXY_API_KEY=
AGENTOS_DEFAULT_MODEL=gpt-5.6-sol
`,
    );
    expect(exitCode).toBe(0);

    expect(stderr).toContain(
      "warning: configured default provider 'codex' disabled because CODEX_PROXY_API_KEY is empty",
    );
    expect(stderr).toContain("unqualified requests can fall back to the Anthropic cloud API");
  });

  it("does not warn about Anthropic cloud routing without configured defaults", async () => {
    const { captured, exitCode, stderr } = await runDevUpFixture("CODEX_PROXY_API_KEY=\n");
    expect(exitCode).toBe(0);
    expect(captured).toEqual({
      codex: null,
      anthropic: null,
      openai: null,
      defaultModel: null,
    });

    expect(stderr).not.toContain("Anthropic cloud");
  });

  it("warns about Anthropic cloud routing when the configured default is disabled", async () => {
    const { exitCode, stderr } = await runDevUpFixture(
      `CODEX_PROXY_API_KEY=
AGENTOS_DEFAULT_PROVIDER=codex
AGENTOS_DEFAULT_MODEL=gpt-5.6-sol
`,
      { ANTHROPIC_API_KEY: "test-cloud-secret" },
    );
    expect(exitCode).toBe(0);

    expect(stderr).toContain("Anthropic cloud");
    expect(stderr).not.toContain("test-cloud-secret");
  });
});

describe("dev-up dotenv allowlist", () => {
  it("accepts the channel secrets the workers actually read", async () => {
    // SLACK_* were missing from .env.example, so a correct Slack token was
    // rejected by the very script that starts the channel workers.
    const { exitCode, stderr } = await runDevUpFixture(
      "SLACK_BOT_TOKEN=xoxb-test\nSLACK_SIGNING_SECRET=signing\nTELEGRAM_BOT_TOKEN=telegram\n",
    );
    expect(stderr).not.toContain("unknown dotenv variable");
    expect(exitCode).toBe(0);
  });

  it("accepts the memworkr runtime names from the template instead of a hand-maintained list", async () => {
    const { exitCode, stderr } = await runDevUpFixture(
      "MEMWORKR_INSTANCE_ID=unitb-test\nIII_WS_URL=ws://127.0.0.1:49134\n",
    );
    expect(stderr).not.toContain("unknown dotenv variable");
    expect(exitCode).toBe(0);
  });

  it("accepts an env key declared by an integration manifest", async () => {
    const { exitCode, stderr } = await runDevUpFixture("EXAMPLE_INTEGRATION_TOKEN=value\n", {}, {
      integrations: { example: ["EXAMPLE_INTEGRATION_TOKEN"] },
    });
    expect(stderr).not.toContain("unknown dotenv variable");
    expect(exitCode).toBe(0);
  });

  it("still refuses a name nothing declares", async () => {
    const { exitCode, stderr } = await runDevUpFixture("NOT_DECLARED_ANYWHERE=value\n", {}, {
      integrations: { example: ["EXAMPLE_INTEGRATION_TOKEN"] },
    });
    expect(exitCode).toBe(1);
    expect(stderr).toContain("error: unknown dotenv variable 'NOT_DECLARED_ANYWHERE' on line 1");
  });

  it("reads integration manifest keys only from the env section", async () => {
    // `id = "example"` sits under [integration], not [integration.env]: a
    // parser that scanned the whole manifest would allow `id` as a variable.
    const { exitCode, stderr } = await runDevUpFixture("id=value\n", {}, {
      integrations: { example: ["INSIDE_ENV"] },
    });
    expect(exitCode).toBe(1);
    expect(stderr).toContain("unknown dotenv variable 'id'");
  });
});

describe("dev-up memworkr runtime", () => {
  it("starts a synced memworkr whose recorded digest matches", async () => {
    const { exitCode, stdout, stderr, memworkrStarted } = await runDevUpFixture("", {}, {
      memworkr: { digest: "correct", healthy: true },
    });
    expect(exitCode).toBe(0);
    expect(memworkrStarted).toBe(true);
    expect(stdout).toContain("memworkr");
    expect(stdout).toContain("ready");
    expect(stderr).not.toContain("memworkr disabled");
  });

  it("refuses to execute a binary that does not match the recorded digest", async () => {
    const { exitCode, stderr, memworkrStarted } = await runDevUpFixture("", {}, {
      memworkr: { digest: "wrong" },
    });
    // The runtime root sits inside the shell worker's jail, so an unverified
    // binary must never be executed - and the rest of the stack must survive.
    expect(memworkrStarted).toBe(false);
    expect(stderr).toContain("digest mismatch");
    expect(exitCode).toBe(0);
  });

  it("refuses a version directory with no recorded digest", async () => {
    const { exitCode, stderr, memworkrStarted } = await runDevUpFixture("", {}, {
      memworkr: { digest: "missing" },
    });
    expect(memworkrStarted).toBe(false);
    expect(stderr).toContain("no recorded digest");
    expect(exitCode).toBe(0);
  });

  it("keeps the stack running when memworkr fails its readiness check", async () => {
    const { exitCode, stderr, captured, memworkrStarted } = await runDevUpFixture(
      "",
      { MEMWORKR_READY_ATTEMPTS: "2" },
      { memworkr: { digest: "correct", healthy: false } },
    );
    // No AgentOS code path calls memory::assert/as_of/provenance, so an
    // unhealthy memworkr must not take 62 workers down with it.
    expect(memworkrStarted).toBe(true);
    expect(stderr).toContain("failed the memory::health readiness check");
    expect(stderr).toContain("the rest of the stack keeps running");
    expect(exitCode).toBe(0);
    expect(captured).not.toBeNull();
    // Two readiness attempts plus a one-second sleep each: the default 5s test
    // budget is too tight once the whole suite runs in parallel.
  }, 30_000);

  it("refuses a current pointer that is not a 40-character commit", async () => {
    const { exitCode, stderr, memworkrStarted } = await runDevUpFixture("", {}, {
      memworkr: { version: "not-a-commit", digest: "correct" },
    });
    expect(memworkrStarted).toBe(false);
    expect(stderr).toContain("not a 40-character commit");
    expect(exitCode).toBe(0);
  });
});

describe("dev-up bus RBAC gate", () => {
  it("starts the bus-auth daemon and does not treat it as a worker", async () => {
    const { exitCode, stdout, busAuth } = await runDevUpFixture("", {}, { busAuth: "present" });

    expect(exitCode).toBe(0);
    expect(busAuth).not.toBeNull();
    // The daemon takes its address from the CLI, not from the environment.
    expect(busAuth?.argv).toContain("--listen=127.0.0.1:49129");
    expect(stdout).toContain("bus-auth daemon listening on 127.0.0.1:49129");
    // It is not a worker: the worker loop must not have started a second copy.
    expect(busAuth?.starts).toBe(1);
  }, 30_000);

  it("warns when the config arms bus RBAC and the daemon is not built", async () => {
    const { exitCode, stderr } = await runDevUpFixture("", {}, { busAuth: "armed-missing" });

    expect(exitCode).toBe(0);
    expect(stderr).toContain("arms bus RBAC");
    expect(stderr).toContain("cargo build --workspace --release");
  });

  it("says nothing when neither the daemon nor the gate is present", async () => {
    const { exitCode, stderr, stdout } = await runDevUpFixture("");

    expect(exitCode).toBe(0);
    expect(stderr).not.toContain("bus RBAC");
    expect(stdout).not.toContain("bus-auth");
  });
});
