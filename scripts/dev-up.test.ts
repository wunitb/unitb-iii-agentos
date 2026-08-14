import { execFile } from "node:child_process";
import { on } from "node:events";
import { watch } from "node:fs";
import { chmod, copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
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

interface FixtureResult {
  captured: CapturedEnvironment;
  stderr: string;
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
): Promise<FixtureResult> {
  const root = await mkdtemp(join(tmpdir(), "agentos-dev-up-"));
  fixtureRoots.push(root);

  const scriptPath = join(root, "scripts", "dev-up.sh");
  const workerPath = join(root, "target", "release", "agentos-env-probe");
  const capturePath = join(root, "captured");

  await mkdir(dirname(scriptPath), { recursive: true });
  await mkdir(dirname(workerPath), { recursive: true });
  await copyFile(new URL("./dev-up.sh", import.meta.url), scriptPath);
  await copyFile(new URL("../.env.example", import.meta.url), join(root, ".env.example"));
  await writeFile(join(root, ".env"), dotenv, { mode: 0o600 });
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

  const watcher = watch(root);
  const changes = on(watcher, "change");

  try {
    const { stderr } = await execFileAsync("bash", [scriptPath], {
      encoding: "utf8",
      env: {
        PATH: process.env.PATH,
        HOME: process.env.HOME,
        TMPDIR: process.env.TMPDIR,
        ...inherited,
      },
    });
    for await (const [, filename] of changes) {
      if (filename?.toString() === "captured") break;
    }
    const [codex, anthropic, openai, defaultModel] = await Promise.all([
      readCapturedValue(capturePath, "codex"),
      readCapturedValue(capturePath, "anthropic"),
      readCapturedValue(capturePath, "openai"),
      readCapturedValue(capturePath, "defaultModel"),
    ]);
    return {
      captured: { codex, anthropic, openai, defaultModel },
      stderr,
    };
  } finally {
    watcher.close();
  }
}

afterEach(async () => {
  await Promise.all(fixtureRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("dev-up dotenv loading", () => {
  it("normalizes surrounding whitespace and matching quotes", async () => {
    const { captured } = await runDevUpFixture(
      `CODEX_PROXY_API_KEY="quoted-secret"
ANTHROPIC_API_KEY=  cloud-secret  
OPENAI_API_KEY='open secret'
`,
    );

    expect(captured).toEqual({
      codex: "quoted-secret",
      anthropic: "cloud-secret",
      openai: "open secret",
      defaultModel: null,
    });
  });

  it("preserves inherited credentials when file values normalize to empty", async () => {
    const { captured } = await runDevUpFixture(
      "CODEX_PROXY_API_KEY=   \nANTHROPIC_API_KEY=\t\n",
      {
        CODEX_PROXY_API_KEY: "inherited-codex",
        ANTHROPIC_API_KEY: "inherited-anthropic",
      },
    );

    expect(captured).toEqual({
      codex: "inherited-codex",
      anthropic: "inherited-anthropic",
      openai: null,
      defaultModel: null,
    });
  });

  it("keeps shell syntax, unmatched quotes, and inline comments inert", async () => {
    const { captured } = await runDevUpFixture(
      `CODEX_PROXY_API_KEY=$(printf exploited)
ANTHROPIC_API_KEY=$HOME
OPENAI_API_KEY="unmatched
AGENTOS_DEFAULT_MODEL=value # literal
`,
    );

    expect(captured).toEqual({
      codex: "$(printf exploited)",
      anthropic: "$HOME",
      openai: '"unmatched',
      defaultModel: "value # literal",
    });
  });

  it("warns about Anthropic cloud routing when the configured default is disabled", async () => {
    const { stderr } = await runDevUpFixture(
      `CODEX_PROXY_API_KEY=
AGENTOS_DEFAULT_PROVIDER=codex
AGENTOS_DEFAULT_MODEL=gpt-5.6-sol
`,
      { ANTHROPIC_API_KEY: "test-cloud-secret" },
    );

    expect(stderr).toContain("Anthropic cloud");
    expect(stderr).not.toContain("test-cloud-secret");
  });
});
