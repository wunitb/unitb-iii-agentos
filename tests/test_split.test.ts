import { describe, expect, it } from "bun:test";
import { readdirSync, statSync } from "node:fs";
import { join } from "node:path";

/**
 * `test:unit` used to run 76 tests, 37 of which asserted on markdown under
 * `docs/builds/` and on README prose. That made a green suite look like product
 * coverage it did not have. (`docs/builds/` itself left this repository on
 * 2026-09-02 — it recorded how work was produced, not what the product does.) The suites are now split: `test:unit` is tests of the
 * software, `test:governance` is build-evidence and documentation contracts, and
 * CI runs both as separate steps.
 *
 * A split is only safe if nothing can fall between the two commands, so this
 * suite re-derives the coverage of both scripts from `package.json` and asserts
 * that together they partition every test file in the repository.
 */

const repositoryRoot = new URL("../", import.meta.url).pathname;
const manifest = await Bun.file(new URL("package.json", new URL("../", import.meta.url))).json();
const SKIP_DIRECTORIES = new Set([".git", ".worktrees", "node_modules", "target", "dist", "e2e"]);
const TEST_ROOTS = ["tests", "examples", "scripts"];

function testFiles(): string[] {
  const found: string[] = [];
  const walk = (relative: string): void => {
    for (const entry of readdirSync(join(repositoryRoot, relative))) {
      if (SKIP_DIRECTORIES.has(entry)) continue;
      const path = join(relative, entry);
      if (statSync(join(repositoryRoot, path)).isDirectory()) walk(path);
      else if (entry.endsWith(".test.ts")) found.push(path);
    }
  };
  for (const root of TEST_ROOTS) walk(root);
  return found.sort();
}

/** The path arguments of a `bun test ...` script. */
function targetsOf(script: string): string[] {
  const match = /^bun test (.+)$/.exec(script.trim());
  if (!match) throw new Error(`not a bun test script: ${script}`);
  return match[1]!.split(/\s+/);
}

/** Reproduce selection for the argument shapes these scripts use: a directory, an
 *  exact file, or a non-recursive `dir/*.suffix` glob. */
function selects(target: string, file: string): boolean {
  const glob = /^(.*)\/\*(\.[A-Za-z.]+)$/.exec(target);
  if (glob) {
    const directory = glob[1]!;
    return (
      file.startsWith(`${directory}/`) &&
      !file.slice(directory.length + 1).includes("/") &&
      file.endsWith(glob[2]!)
    );
  }
  return file === target || file.startsWith(`${target}/`);
}

function covers(script: string, file: string): boolean {
  return targetsOf(script).some((target) => selects(target, file));
}

const unitScript: string = manifest.scripts["test:unit"];
const governanceScript: string = manifest.scripts["test:governance"];
const files = testFiles();

describe("test suite split", () => {
  it("defines both suites and keeps `test` pointing at the software suite", () => {
    expect(typeof unitScript).toBe("string");
    expect(typeof governanceScript).toBe("string");
    expect(manifest.scripts.test).toBe("bun run test:unit");
    expect(manifest.scripts.check).toContain("test:governance");
    expect(manifest.scripts.check).toContain("counts:check");
  });

  it("covers every test file exactly once across the two suites", () => {
    expect(files.length).toBeGreaterThan(10);
    const uncovered = files.filter((file) => !covers(unitScript, file) && !covers(governanceScript, file));
    const both = files.filter((file) => covers(unitScript, file) && covers(governanceScript, file));

    expect(uncovered, "these test files run in no CI step").toEqual([]);
    expect(both, "these test files run twice").toEqual([]);
  });

  it("routes governance evidence out of the software suite", () => {
    const governance = files.filter((file) => covers(governanceScript, file));
    expect(governance).toContain("tests/governance/reconciliation_contract.test.ts");
    expect(governance).toContain("tests/governance/quickstart.test.ts");
    for (const file of governance) {
      expect(covers(unitScript, file), `${file} still runs inside test:unit`).toBe(false);
    }
  });

  it("keeps the product tests in the software suite", () => {
    const unit = files.filter((file) => covers(unitScript, file));
    for (const expected of [
      "tests/config_boot.test.ts",
      "tests/llm_namespace.test.ts",
      "tests/install_upgrade.test.ts",
      "tests/state_protocol.test.ts",
      "tests/registration_uniqueness.test.ts",
      "examples/shared.test.ts",
      "scripts/dev-up.test.ts",
    ]) {
      expect(unit, `${expected} must stay in test:unit`).toContain(expected);
    }
  });

  it("typechecks every file it executes", () => {
    // scripts/desktop-up.test.ts, memworkr-sync.test.ts and env-contract.test.ts
    // were executed by `bun test` for a while but invisible to `typecheck:scripts`,
    // which named one file. A test that runs but is never type-checked is a
    // silently weaker gate, so the two lists are tied together here.
    const typecheckTargets = Object.entries(manifest.scripts as Record<string, string>)
      .filter(([name]) => name.startsWith("typecheck:"))
      .flatMap(([, script]) => script.split(/\s+/).filter((token) => /\.(ts|tsx)$/.test(token)));
    expect(typecheckTargets.length, "no typecheck script names any file").toBeGreaterThan(0);

    const untyped = files.filter((file) => !typecheckTargets.some((target) => selects(target, file)));
    expect(untyped, "these test files run in CI but are never type-checked").toEqual([]);
  });

  it("runs both suites in CI as separate, non-optional steps", async () => {
    const workflow = await Bun.file(new URL("../.github/workflows/ci.yml", import.meta.url)).text();
    for (const command of ["bun run test:unit", "bun run test:governance", "bun run counts:check"]) {
      expect(workflow.includes(`run: ${command}`), `ci.yml does not run \`${command}\``).toBe(true);
    }

    const bypassed: string[] = [];
    for (const file of ["ci.yml", "release.yml", "vercel-deploy.yml"]) {
      const source = await Bun.file(new URL(`../.github/workflows/${file}`, import.meta.url)).text();
      // The YAML key, not the word in a comment.
      if (/^\s*continue-on-error\s*:/m.test(source)) bypassed.push(file);
    }
    expect(bypassed, "a workflow made a gate optional").toEqual([]);
  });
});
