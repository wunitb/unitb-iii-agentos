import { describe, expect, it } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const repository = new URL("../", import.meta.url);

function gitAt(cwd: string, ...args: string[]) {
  return Bun.spawnSync({
    cmd: ["git", ...args],
    cwd,
    stdout: "pipe",
    stderr: "pipe",
  });
}

function git(...args: string[]) {
  return gitAt(repository.pathname, ...args);
}

describe("root Bun dependency state", () => {
  it("commits one non-empty root Bun lockfile", async () => {
    const tracked = git("ls-files", "--error-unmatch", "--", "bun.lock");

    expect(tracked.exitCode).toBe(0);
    expect(tracked.stdout.toString().trim().split("\n")).toEqual(["bun.lock"]);

    const lockfile = await Bun.file(new URL("bun.lock", repository)).text();
    expect(lockfile.length).toBeGreaterThan(0);
    expect(lockfile).toContain('"lockfileVersion": 1');
    expect(lockfile.trimEnd().endsWith("}")).toBe(true);
  });

  it("locks every direct package dependency at the workspace root", async () => {
    const manifest = await Bun.file(new URL("package.json", repository)).json();
    const lockfile = await Bun.file(new URL("bun.lock", repository)).text();
    const rootWorkspace = lockfile.slice(
      lockfile.indexOf('"workspaces"'),
      lockfile.indexOf('"packages"'),
    );

    for (const dependencies of [
      manifest.dependencies ?? {},
      manifest.devDependencies ?? {},
    ]) {
      for (const [name, version] of Object.entries(dependencies)) {
        expect(rootWorkspace, `${name} is absent from the root lock entry`).toContain(
          `${JSON.stringify(name)}: ${JSON.stringify(version)}`,
        );
      }
    }
  });

  it("does not hide AgentField runner output in repository policy", async () => {
    const temporaryRepository = await mkdtemp(join(tmpdir(), "agentos-ignore-policy-"));
    try {
      expect(gitAt(temporaryRepository, "init", "--quiet").exitCode).toBe(0);
      await writeFile(
        join(temporaryRepository, ".gitignore"),
        await Bun.file(new URL(".gitignore", repository)).text(),
      );

      const runnerOutput = gitAt(
        temporaryRepository,
        "-c",
        "core.excludesFile=/dev/null",
        "check-ignore",
        "--no-index",
        "--",
        ".agentfield-out-test/result.json",
      );
      expect(runnerOutput.exitCode).toBe(1);
      expect(runnerOutput.stdout.toString()).toBe("");
    } finally {
      await rm(temporaryRepository, { recursive: true, force: true });
    }
  });
});
