import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { access, chmod, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { afterEach, describe, expect, it } from "vitest";

const execFileAsync = promisify(execFile);
const fixtureRoots: string[] = [];

interface RunResult {
  exitCode: number;
  stdout: string;
  stderr: string;
}

interface Fixture {
  /// Where the script keeps versions and the `current` pointer.
  runtimeRoot: string;
  /// A clean memworkr checkout stand-in with a passing release gate.
  source: string;
  /// HEAD of the fake checkout, i.e. the installed version name.
  commit: string;
  run: (...args: string[]) => Promise<RunResult>;
  versionDir: (commit?: string) => string;
}

async function git(cwd: string, ...args: string[]): Promise<string> {
  const { stdout } = await execFileAsync("git", ["-C", cwd, ...args], {
    encoding: "utf8",
    env: {
      PATH: process.env.PATH,
      HOME: process.env.HOME,
      GIT_AUTHOR_NAME: "fixture",
      GIT_AUTHOR_EMAIL: "fixture@example.invalid",
      GIT_COMMITTER_NAME: "fixture",
      GIT_COMMITTER_EMAIL: "fixture@example.invalid",
    },
  });
  return stdout.trim();
}

/// A sandbox holding the script under test, a fake memworkr checkout whose
/// release gate passes, and an empty runtime root.
async function fixture(binaryBody = "#!/bin/sh\nexit 0\n"): Promise<Fixture> {
  const root = await mkdtemp(join(tmpdir(), "agentos-memworkr-sync-"));
  fixtureRoots.push(root);

  const scriptPath = join(root, "scripts", "memworkr-sync.sh");
  await mkdir(join(root, "scripts"), { recursive: true });
  await writeFile(
    scriptPath,
    await readFile(new URL("./memworkr-sync.sh", import.meta.url), "utf8"),
  );

  const source = join(root, "source");
  await mkdir(join(source, "scripts"), { recursive: true });
  await mkdir(join(source, "target", "release"), { recursive: true });
  await writeFile(join(source, "Cargo.lock"), "# fixture\n");
  await writeFile(join(source, "scripts", "release-gate.sh"), "#!/bin/sh\nexit 0\n");
  await chmod(join(source, "scripts", "release-gate.sh"), 0o755);
  await writeFile(join(source, "target", "release", "memworkr"), binaryBody);
  await chmod(join(source, "target", "release", "memworkr"), 0o755);
  await writeFile(join(source, ".gitignore"), "target/\n");

  await git(source, "init", "-q");
  await git(source, "add", "-A");
  await git(source, "commit", "-q", "-m", "fixture");
  const commit = await git(source, "rev-parse", "HEAD");

  const runtimeRoot = join(root, "runtime");
  const run = async (...args: string[]): Promise<RunResult> => {
    try {
      const result = await execFileAsync("bash", [scriptPath, ...args], {
        encoding: "utf8",
        env: {
          PATH: process.env.PATH,
          HOME: process.env.HOME,
          MEMWORKR_RUNTIME_ROOT: runtimeRoot,
        },
      });
      return { exitCode: 0, stdout: result.stdout, stderr: result.stderr };
    } catch (error) {
      const failure = error as { code?: number | string | null; stdout?: string; stderr?: string };
      if (typeof failure.code !== "number") throw error;
      return { exitCode: failure.code, stdout: failure.stdout ?? "", stderr: failure.stderr ?? "" };
    }
  };

  return {
    runtimeRoot,
    source,
    commit,
    run,
    versionDir: (version = commit) => join(runtimeRoot, "versions", version),
  };
}

async function exists(path: string): Promise<boolean> {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
}

afterEach(async () => {
  await Promise.all(fixtureRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("memworkr-sync install", () => {
  it("records the digest of the binary it installed", async () => {
    const context = await fixture();
    const sync = await context.run("sync", context.source);
    expect(sync.exitCode).toBe(0);

    const installed = await readFile(join(context.versionDir(), "memworkr"));
    const recorded = (await readFile(join(context.versionDir(), "SHA256"), "utf8")).trim();
    expect(recorded).toBe(createHash("sha256").update(installed).digest("hex"));
    expect((await readFile(join(context.versionDir(), "VERSION"), "utf8")).trim()).toBe(
      context.commit,
    );
    expect((await readFile(join(context.runtimeRoot, "current"), "utf8")).trim()).toBe(
      context.commit,
    );
    expect((await context.run("status")).exitCode).toBe(0);
  });

  it("replaces a wedged version directory instead of nesting the staging directory inside it", async () => {
    const context = await fixture();
    // An interrupted install leaves a directory with no usable binary. POSIX
    // `mv` would move the staging directory *into* it, so every later sync
    // nests another `.sync.*` and `activate` fails forever.
    await mkdir(context.versionDir(), { recursive: true });
    await writeFile(join(context.versionDir(), "memworkr"), "truncated");
    await chmod(join(context.versionDir(), "memworkr"), 0o644);

    const sync = await context.run("sync", context.source);
    expect(sync.exitCode).toBe(0);

    const entries = await readdir(context.versionDir());
    expect(entries.sort()).toEqual(["SHA256", "VERSION", "memworkr"]);
    expect(entries.some((entry) => entry.startsWith(".sync."))).toBe(false);
    expect((await context.run("status")).exitCode).toBe(0);
  });

  it("re-runs the gate when an installed version has no recorded digest", async () => {
    const context = await fixture();
    await context.run("sync", context.source);
    await rm(join(context.versionDir(), "SHA256"));

    const sync = await context.run("sync", context.source);
    expect(sync.exitCode).toBe(0);
    expect(await exists(join(context.versionDir(), "SHA256"))).toBe(true);
  });
});

describe("memworkr-sync verification", () => {
  it("refuses to activate a binary that does not match its recorded digest", async () => {
    const context = await fixture();
    await context.run("sync", context.source);
    // The runtime root sits inside the shell worker's jail root, so a confined
    // write can reach this file; the digest is what makes that write inert.
    await writeFile(join(context.versionDir(), "memworkr"), "#!/bin/sh\nexit 7\n");
    await chmod(join(context.versionDir(), "memworkr"), 0o755);

    const rollback = await context.run("rollback", context.commit);
    expect(rollback.exitCode).toBe(1);
    expect(rollback.stderr).toContain("digest mismatch");

    const status = await context.run("status");
    expect(status.exitCode).toBe(1);
    expect(status.stderr).toContain("digest mismatch");
  });

  it("refuses a rollback to a version with no recorded digest", async () => {
    const context = await fixture();
    await context.run("sync", context.source);
    await rm(join(context.versionDir(), "SHA256"));

    const rollback = await context.run("rollback", context.commit);
    expect(rollback.exitCode).toBe(1);
    expect(rollback.stderr).toContain("no recorded digest");
  });

  it("keeps refusing a dirty source checkout and a non-checkout path", async () => {
    const context = await fixture();
    await writeFile(join(context.source, "dirty.txt"), "uncommitted\n");
    const dirty = await context.run("sync", context.source);
    expect(dirty.exitCode).toBe(1);
    expect(dirty.stderr).toContain("dirty memworkr checkout");

    const wrong = await context.run("sync", context.runtimeRoot);
    expect(wrong.exitCode).toBe(1);
  });
});
