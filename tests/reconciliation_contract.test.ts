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

function expectAncestor(ancestor: string): void {
  const resolved = git("rev-parse", "--verify", `${ancestor}^{commit}`);
  expect(
    resolved.exitCode,
    `required ancestor ${ancestor} is unavailable: ${resolved.stderr.toString()}`,
  ).toBe(0);

  const result = git("merge-base", "--is-ancestor", ancestor, "HEAD");
  expect(
    result.exitCode,
    `${ancestor} is not an ancestor of HEAD: ${result.stderr.toString()}`,
  ).toBe(0);
}

function conflictMarkerLines(source: string): string[] {
  const left = "<".repeat(7);
  const divider = "=".repeat(7);
  const right = ">".repeat(7);
  return source
    .split(/\r?\n/)
    .filter(
      (line) =>
        line === divider ||
        line === left ||
        line.startsWith(`${left} `) ||
        line === right ||
        line.startsWith(`${right} `),
    );
}

describe("reconciled Git history and worktree contract", () => {
  it("retains main, remediation, and the Herdr fleet tip as ancestors", () => {
    for (const ancestor of [
      "origin/main",
      "origin/issue/1897372b-remediate-artifact-1",
      "238b423",
    ]) {
      expectAncestor(ancestor);
    }
  });

  it("contains no conflict-marker line in tracked or visible untracked files", async () => {
    const listed = git(
      "ls-files",
      "--cached",
      "--others",
      "--exclude-standard",
      "-z",
    );
    expect(listed.exitCode, listed.stderr.toString()).toBe(0);

    const failures: string[] = [];
    for (const path of listed.stdout.toString().split("\0").filter(Boolean)) {
      const markers = conflictMarkerLines(
        await Bun.file(new URL(path, repository)).text(),
      );
      if (markers.length > 0) failures.push(`${path}: ${markers.join(", ")}`);
    }
    expect(failures).toEqual([]);
  });
});

describe("conflict-marker detector edge cases", () => {
  it("accepts empty input and marker-like prose", () => {
    expect(conflictMarkerLines("")).toEqual([]);
    const nearMisses = [
      `prefix ${"=".repeat(7)} suffix`,
      `${"<".repeat(6)} six`,
      `${">".repeat(8)} eight`,
    ].join("\n");
    expect(conflictMarkerLines(nearMisses)).toEqual([]);
  });

  it("detects all marker forms at line and file boundaries", () => {
    const left = "<".repeat(7);
    const divider = "=".repeat(7);
    const right = ">".repeat(7);
    expect(
      conflictMarkerLines(
        [`${left} ours`, divider, `${right} theirs`, left, right].join("\n"),
      ),
    ).toEqual([`${left} ours`, divider, `${right} theirs`, left, right]);
  });
});
