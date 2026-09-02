import { describe, expect, it } from "bun:test";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

/**
 * The 0.11.6 -> 0.22.1 engine upgrade deleted upstream's workspace dependency
 * inheritance: on 2026-09-02 all 62 workers hardcoded `iii-sdk = "=0.22.1"`,
 * `tokio`, `serde`, `edition` and `license`, and the root
 * `[workspace.dependencies]` table (Cargo.toml:76-84) was dead configuration that
 * nothing inherited. That is why a version bump was a 62-file sweep.
 *
 * This suite asserts the invariant, not the spelling: one declaration per shared
 * dependency for the whole workspace, inherited everywhere it is used. It also
 * locks the two things that depend on it — `publish = false` (without which
 * cargo-deny cannot excuse the `agentos-http-adapter` path wildcard) and the
 * anchored `CLAUDE.md` ignore rule that had silently dropped `identity/CLAUDE.md`
 * out of the tree.
 */

const repository = new URL("../", import.meta.url);
const repositoryPath = fileURLToPath(repository);

/** A dependency used by this many members or more must live in the workspace table. */
const SHARED_THRESHOLD = 3;

type DependencySpec = string | Record<string, unknown>;

interface Manifest {
  readonly package?: Record<string, unknown>;
  readonly dependencies?: Record<string, DependencySpec>;
  readonly "dev-dependencies"?: Record<string, DependencySpec>;
  readonly "build-dependencies"?: Record<string, DependencySpec>;
  readonly workspace?: {
    readonly members?: string[];
    readonly package?: Record<string, unknown>;
    readonly dependencies?: Record<string, DependencySpec>;
  };
}

async function manifest(path: string): Promise<Manifest> {
  return Bun.TOML.parse(await Bun.file(new URL(path, repository)).text()) as Manifest;
}

const workspaceManifest = await manifest("Cargo.toml");
const memberNames = workspaceManifest.workspace?.members ?? [];
const members = await Promise.all(
  memberNames.map(async (member) => ({
    member,
    path: `${member}/Cargo.toml`,
    manifest: await manifest(`${member}/Cargo.toml`),
  })),
);

function inherits(spec: DependencySpec | undefined): boolean {
  return typeof spec === "object" && spec !== null && (spec as { workspace?: boolean }).workspace === true;
}

function dependencyTables(parsed: Manifest): Array<Record<string, DependencySpec>> {
  return [parsed.dependencies, parsed["dev-dependencies"], parsed["build-dependencies"]].filter(
    (table): table is Record<string, DependencySpec> => typeof table === "object" && table !== null,
  );
}

/** member path -> dependency name -> declaration, across all three dependency tables. */
function declarations(parsed: Manifest): Map<string, DependencySpec> {
  const found = new Map<string, DependencySpec>();
  for (const table of dependencyTables(parsed)) {
    for (const [name, spec] of Object.entries(table)) found.set(name, spec);
  }
  return found;
}

function git(...args: string[]): { status: number | null; stdout: string } {
  const result = spawnSync("git", args, { cwd: repositoryPath, encoding: "utf8" });
  return { status: result.status, stdout: result.stdout ?? "" };
}

describe("workspace manifest inheritance", () => {
  it("enumerates a real workspace", () => {
    expect(members.length).toBeGreaterThan(60);
    expect(memberNames).toContain("workers/memory");
    expect(memberNames).toContain("crates/http-adapter");
  });

  it("declares the shared package metadata once and inherits it everywhere", () => {
    const workspacePackage = workspaceManifest.workspace?.package ?? {};
    expect(workspacePackage.edition).toBe("2024");
    expect(workspacePackage.license).toBe("Apache-2.0");
    expect(workspacePackage["rust-version"]).toBe("1.90");
    expect(
      workspacePackage.publish,
      "cargo-deny's allow-wildcard-paths only excuses unpublishable crates",
    ).toBe(false);

    const offenders: string[] = [];
    for (const { path, manifest: parsed } of members) {
      const pkg = parsed.package ?? {};
      for (const key of ["version", "edition", "license", "publish"]) {
        if (!inherits(pkg[key] as DependencySpec | undefined)) {
          offenders.push(`${path}: ${key} = ${JSON.stringify(pkg[key])} (want ${key}.workspace = true)`);
        }
      }
      const rustVersion = pkg["rust-version"];
      if (rustVersion !== undefined && !inherits(rustVersion as DependencySpec)) {
        offenders.push(`${path}: rust-version = ${JSON.stringify(rustVersion)} (want rust-version.workspace = true)`);
      }
    }
    expect(offenders, "these members copy workspace package metadata instead of inheriting it").toEqual([]);
  });

  it("inherits every centralised dependency instead of re-pinning it", () => {
    const shared = workspaceManifest.workspace?.dependencies ?? {};
    const offenders: string[] = [];
    for (const { path, manifest: parsed } of members) {
      for (const [name, spec] of declarations(parsed)) {
        if (!(name in shared)) continue;
        if (inherits(spec)) continue;
        offenders.push(`${path}: ${name} = ${JSON.stringify(spec)} shadows [workspace.dependencies]`);
      }
    }
    expect(
      offenders,
      "a member that re-pins a centralised dependency puts the workspace back to a per-member sweep",
    ).toEqual([]);
  });

  it("centralises every dependency three or more members share", () => {
    const shared = workspaceManifest.workspace?.dependencies ?? {};
    const users = new Map<string, string[]>();
    for (const { path, manifest: parsed } of members) {
      for (const name of declarations(parsed).keys()) {
        users.set(name, [...(users.get(name) ?? []), path]);
      }
    }

    const missing: string[] = [];
    for (const [name, paths] of users) {
      if (name in shared) continue;
      if (paths.length < SHARED_THRESHOLD) continue;
      missing.push(`${name} is declared by ${paths.length} members but not in [workspace.dependencies]`);
    }
    expect(missing, "move these into the root workspace table").toEqual([]);
  });

  it("keeps no dead entry in the workspace dependency table", () => {
    const shared = Object.keys(workspaceManifest.workspace?.dependencies ?? {});
    expect(shared.length).toBeGreaterThan(10);

    const inherited = new Set<string>();
    for (const { manifest: parsed } of members) {
      for (const [name, spec] of declarations(parsed)) {
        if (inherits(spec)) inherited.add(name);
      }
    }
    const dead = shared.filter((name) => !inherited.has(name));
    expect(dead, "these workspace dependencies are configuration nothing reads").toEqual([]);
  });

  it("resolves one version per centralised dependency in Cargo.lock", async () => {
    // The point of the table is a single pin. Read it back out of the lockfile so
    // the assertion survives a member adding a conflicting requirement elsewhere.
    const lockfile = await Bun.file(new URL("Cargo.lock", repository)).text();
    const locked = new Map<string, Set<string>>();
    for (const entry of lockfile.matchAll(/\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"/g)) {
      const versions = locked.get(entry[1]!) ?? new Set<string>();
      versions.add(entry[2]!);
      locked.set(entry[1]!, versions);
    }

    for (const name of ["iii-sdk", "tokio", "serde", "serde_json", "reqwest", "chrono", "uuid"]) {
      const versions = locked.get(name);
      expect(versions, `${name} is not in Cargo.lock`).toBeDefined();
      expect([...versions!], `${name} resolves to more than one version`).toHaveLength(1);
    }
  });
});

describe("identity bundle", () => {
  it("anchors the CLAUDE.md ignore rule to the repository root", async () => {
    const gitignore = await Bun.file(new URL(".gitignore", repository)).text();
    const rules = gitignore.split("\n").map((line) => line.trim());
    expect(rules, "a bare CLAUDE.md matches at any depth and hides tracked content").not.toContain("CLAUDE.md");
    expect(rules).toContain("/CLAUDE.md");
  });

  it("tracks identity/CLAUDE.md", () => {
    const ignored = git("check-ignore", "-v", "identity/CLAUDE.md");
    expect(ignored.status, `identity/CLAUDE.md is ignored by ${ignored.stdout.trim()}`).not.toBe(0);

    const tracked = git("ls-files", "--", "identity/");
    const files = tracked.stdout.split("\n").filter(Boolean);
    expect(tracked.status).toBe(0);
    expect(files, "the upstream identity bundle is incomplete").toContain("identity/CLAUDE.md");
    expect(files.length, "identity/ should carry the whole upstream bundle").toBe(9);
  });

  it("leaves every other CLAUDE.md out of the tree", () => {
    const tracked = git("ls-files", "--", "*CLAUDE.md");
    expect(tracked.stdout.split("\n").filter(Boolean)).toEqual(["identity/CLAUDE.md"]);
  });
});
