import { describe, expect, it } from "bun:test";

const repository = new URL("../", import.meta.url);

function git(...args: string[]) {
  return Bun.spawnSync({
    cmd: ["git", ...args],
    cwd: repository.pathname,
    stdout: "pipe",
    stderr: "pipe",
  });
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

  it("does not hide AgentField runner output in repository policy", () => {
    const runnerOutput = git(
      "check-ignore",
      "--no-index",
      "--",
      ".agentfield-out-test/result.json",
    );
    expect(runnerOutput.exitCode).toBe(1);
    expect(runnerOutput.stdout.toString()).toBe("");
  });
});
