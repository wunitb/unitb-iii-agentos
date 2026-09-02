import { describe, expect, it } from "bun:test";

const repository = new URL("../../", import.meta.url);

function git(...args: string[]) {
  return Bun.spawnSync({
    cmd: ["git", ...args],
    cwd: repository.pathname,
    stdout: "pipe",
    stderr: "pipe",
  });
}

const reconciledArtifacts = [
  // The build-artifact directories that used to be asserted here moved out of this
  // public repository on 2026-09-02: they are a record of how work was produced,
  // not of what the product does. The delivered CONTENT is still pinned below.
  { path: "docs/decisions/2026-08-22-salvage-batch.md" },
  { path: "workers/llm-router/src/main.rs", requiredContent: "agentos::llm" },
  { path: "crates/cli/src/bootstrap.rs", requiredContent: "connected_worker_ids" },
] as const;

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
  it("retains the reconciled salvage, namespace migration, and fail-closed startup work", () => {
    for (const artifact of reconciledArtifacts) {
      const result = git("cat-file", "blob", `HEAD:${artifact.path}`);
      expect(
        result.exitCode,
        `required reconciled artifact is absent from HEAD: ${artifact.path}\n${result.stderr.toString()}`,
      ).toBe(0);

      const content = result.stdout.toString();
      expect(
        content.length,
        `required reconciled artifact is empty at HEAD: ${artifact.path}`,
      ).toBeGreaterThan(0);
      if ("requiredContent" in artifact) {
        expect(
          content,
          `required reconciled content is absent at HEAD: ${artifact.requiredContent} in ${artifact.path}`,
        ).toContain(artifact.requiredContent);
      }
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
      const file = Bun.file(new URL(path, repository));
        if (!(await file.exists())) continue;
      const markers = conflictMarkerLines(await file.text());
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
