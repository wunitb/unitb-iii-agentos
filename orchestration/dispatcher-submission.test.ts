import { afterEach, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { changedPathsSince } from "./dispatcher";

const roots: string[] = [];

const gitEnvironment = {
  ...process.env,
  GIT_AUTHOR_NAME: "UnitB Fleet Test",
  GIT_AUTHOR_EMAIL: "fleet-test@unitb.local",
  GIT_COMMITTER_NAME: "UnitB Fleet Test",
  GIT_COMMITTER_EMAIL: "fleet-test@unitb.local",
};

afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

function git(cwd: string, ...args: string[]): string {
  const result = Bun.spawnSync(["git", ...args], {
    cwd,
    env: gitEnvironment,
    stdout: "pipe",
    stderr: "pipe",
  });
  if (result.exitCode !== 0) throw new Error(result.stderr.toString().trim());
  return result.stdout.toString().trim();
}

test("changed paths expose both sides of a rename across an ownership boundary", async () => {
  const root = mkdtempSync(join(tmpdir(), "unitb-fleet-submission-"));
  roots.push(root);
  mkdirSync(join(root, "outside"));
  writeFileSync(join(root, "outside", "secret.ts"), "export const secret = true;\n");
  git(root, "init", "--quiet");
  git(root, "add", ".");
  git(root, "commit", "--quiet", "-m", "base");
  const base = git(root, "rev-parse", "HEAD");

  mkdirSync(join(root, "owned"));
  git(root, "mv", "outside/secret.ts", "owned/secret.ts");
  git(root, "commit", "--quiet", "-m", "move across boundary");
  const head = git(root, "rev-parse", "HEAD");

  expect(await changedPathsSince(root, base, head)).toEqual([
    "outside/secret.ts",
    "owned/secret.ts",
  ]);
});
