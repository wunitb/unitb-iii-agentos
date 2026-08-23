import { execFile } from "node:child_process";
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

interface FixtureOptions {
  dotenvMode?: number;
  dotenvSymlink?: boolean;
}

interface FixtureResult {
  captured: CapturedEnvironment | null;
  stderr: string;
  exitCode: number;
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
  let exitCode = 0;
  try {
    const result = await execFileAsync("bash", [scriptPath], {
      encoding: "utf8",
      env: {
        PATH: process.env.PATH,
        HOME: process.env.HOME,
        TMPDIR: process.env.TMPDIR,
        ...inherited,
      },
    });
    stderr = result.stderr;
  } catch (error) {
    const commandError = error as {
      code?: number | string | null;
      stderr?: string;
    };
    if (typeof commandError.code !== "number") throw error;
    exitCode = commandError.code;
    stderr = commandError.stderr ?? "";
  }
  if (exitCode !== 0) {
    return { captured: null, stderr, exitCode };
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
    exitCode,
  };
}

afterEach(async () => {
  await Promise.all(fixtureRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
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
