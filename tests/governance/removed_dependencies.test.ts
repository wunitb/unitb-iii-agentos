import { describe, expect, it } from "bun:test";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

/**
 * Removals that are only a docs edit come back on the next docs edit.
 *
 * Clawith is a third-party product with no code coupling to AgentOS. .W ordered
 * it out of this repository on 2026-09-02. Re-adding it is a decision that needs
 * .W, not a paragraph — so the absence is a test, and the failure message says
 * who has to agree before it can return.
 */

const repositoryRoot = fileURLToPath(new URL("../../", import.meta.url));
const selfPath = "tests/governance/removed_dependencies.test.ts";

/** Tracked files, minus the ones whose bytes are not text. */
function trackedTextFiles(): string[] {
  const listed = Bun.spawnSync({
    cmd: ["git", "ls-files", "-z"],
    cwd: repositoryRoot,
    stdout: "pipe",
    stderr: "pipe",
  });
  expect(listed.exitCode, listed.stderr.toString()).toBe(0);

  const skipExtensions = [
    ".png", ".jpg", ".jpeg", ".gif", ".webp", ".ico", ".pdf",
    ".woff", ".woff2", ".ttf", ".otf", ".wasm", ".tar.gz", ".zip",
  ];
  return listed.stdout
    .toString()
    .split("\0")
    .filter((path) => path.length > 0)
    .filter((path) => !skipExtensions.some((extension) => path.endsWith(extension)));
}

function read(path: string): string {
  try {
    return readFileSync(`${repositoryRoot}${path}`, "utf8");
  } catch {
    return "";
  }
}

describe("removed dependencies stay removed", () => {
  it("mentions Clawith nowhere in the tracked tree", () => {
    const offenders: string[] = [];
    for (const path of trackedTextFiles()) {
      if (path === selfPath) continue;
      const source = read(path);
      if (!source) continue;
      const lines = source.split("\n");
      for (const [index, line] of lines.entries()) {
        if (/clawith/i.test(line)) offenders.push(`${path}:${index + 1}: ${line.trim().slice(0, 100)}`);
      }
    }

    expect(
      offenders,
      "Clawith is a third-party product with no code coupling to AgentOS. It was removed from this " +
        "repository on .W's instruction (2026-09-02). Putting it back is .W's decision, not a docs edit — " +
        "if it genuinely belongs here again, get that decision first and then delete this test with it.",
    ).toEqual([]);
  });

  it("scans a meaningful number of files, so an empty result is not an empty scan", () => {
    const files = trackedTextFiles();
    expect(files.length).toBeGreaterThan(100);
    expect(files).toContain("INSTALL_STACK.md");
    expect(files).toContain("README.md");
  });

  it("would catch the string if it came back", () => {
    // The check is a plain case-insensitive match, asserted here so nobody has to
    // trust the loop above.
    for (const sample of ["Clawith", "clawith", "CLAWITH", "clawith-upstream/"]) {
      expect(/clawith/i.test(sample)).toBe(true);
    }
    expect(/clawith/i.test("claw with")).toBe(false);
  });
});

/**
 * The install guide names the repositories an operator has to clone. A third-party
 * product appearing in that list is exactly how the Clawith coupling arrived, so
 * the list is an allowlist with a reason per entry rather than free prose.
 *
 * It is an allowlist, not a fixed count: `iii-hq/iii` is the engine this product
 * runs on and is legitimately named there.
 */
const ALLOWED_REPOSITORIES = new Map([
  ["wunitb/unitb-iii-agentos", "this repository"],
  ["wunitb/unitb-iii-memworkr", "the memory worker the stack installs alongside it"],
  ["iii-hq/iii", "the upstream engine and SDK this product runs on"],
]);

const REQUIRED_REPOSITORIES = ["wunitb/unitb-iii-agentos", "wunitb/unitb-iii-memworkr"];

describe("install guide repository list", () => {
  const guide = read("INSTALL_STACK.md");
  const named = [
    ...new Set(
      [...guide.matchAll(/github\.com\/([A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+?)(?:\.git)?(?=[)\s"'`]|$)/gm)].map(
        (match) => match[1]!,
      ),
    ),
  ].sort();

  it("names the repositories the product actually needs", () => {
    expect(guide.length, "INSTALL_STACK.md is missing").toBeGreaterThan(0);
    for (const required of REQUIRED_REPOSITORIES) {
      expect(named, `INSTALL_STACK.md no longer names ${required}`).toContain(required);
    }
  });

  it("names no repository that has not been justified", () => {
    const unexpected = named.filter((repository) => !ALLOWED_REPOSITORIES.has(repository));
    expect(
      unexpected,
      "a repository in the install guide that is not in the allowlist above. Adding one means the stack " +
        "grew a dependency: record why in ALLOWED_REPOSITORIES, or take it out of the guide.",
    ).toEqual([]);
  });
});
