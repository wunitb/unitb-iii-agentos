import { execFile } from "node:child_process";
import { watch } from "node:fs";
import { chmod, copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { promisify } from "node:util";
import { afterEach, describe, expect, it } from "vitest";

const execFileAsync = promisify(execFile);
const fixtureRoots: string[] = [];

async function runDevUpFixture(
  dotenv: string,
  inherited: NodeJS.ProcessEnv = {},
): Promise<Record<string, string | null>> {
  const root = await mkdtemp(join(tmpdir(), "agentos-dev-up-"));
  fixtureRoots.push(root);

  const scriptPath = join(root, "scripts", "dev-up.sh");
  const workerPath = join(root, "target", "release", "agentos-env-probe");
  const capturePath = join(root, "captured.json");

  await mkdir(dirname(scriptPath), { recursive: true });
  await mkdir(dirname(workerPath), { recursive: true });
  await copyFile(new URL("./dev-up.sh", import.meta.url), scriptPath);
  await copyFile(new URL("../.env.example", import.meta.url), join(root, ".env.example"));
  await writeFile(join(root, ".env"), dotenv, { mode: 0o600 });
  await writeFile(
    workerPath,
    `#!/usr/bin/env bun
import { rename } from "node:fs/promises";
const capturePath = ${JSON.stringify(capturePath)};
await Bun.write(\`\${capturePath}.tmp\`, JSON.stringify({
  codex: process.env.CODEX_PROXY_API_KEY ?? null,
  anthropic: process.env.ANTHROPIC_API_KEY ?? null,
  openai: process.env.OPENAI_API_KEY ?? null,
}));
await rename(\`\${capturePath}.tmp\`, capturePath);
`,
  );
  await chmod(workerPath, 0o755);

  const captureObserved = Promise.withResolvers<void>();
  const watcher = watch(root, (_eventType, filename) => {
    if (filename?.toString() === "captured.json") captureObserved.resolve();
  });
  watcher.once("error", captureObserved.reject);

  try {
    await execFileAsync("bash", [scriptPath], {
      env: { ...process.env, ...inherited },
    });
    await captureObserved.promise;
    return JSON.parse(await readFile(capturePath, "utf8"));
  } finally {
    watcher.close();
  }
}

afterEach(async () => {
  await Promise.all(fixtureRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("dev-up dotenv loading", () => {
  it("normalizes surrounding whitespace and matching quotes", async () => {
    const captured = await runDevUpFixture(
      `CODEX_PROXY_API_KEY="quoted-secret"
ANTHROPIC_API_KEY=  cloud-secret  
OPENAI_API_KEY='open secret'
`,
    );

    expect(captured).toEqual({
      codex: "quoted-secret",
      anthropic: "cloud-secret",
      openai: "open secret",
    });
  });

  it("preserves inherited credentials when file values normalize to empty", async () => {
    const captured = await runDevUpFixture(
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
    });
  });
});
