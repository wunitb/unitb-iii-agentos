import { describe, expect, it } from "bun:test";

/**
 * `deny.toml` is a ratchet, not a snapshot. On 2026-09-02 the workspace resolved
 * 612 packages with 23 crate names on more than one version on the shipped
 * targets, including two crypto stacks (sha2 0.10 + 0.11, digest 0.10 + 0.11).
 * Recording those as dated, justified exceptions makes the debt visible on every
 * pull request; this suite makes sure the exceptions cannot rot:
 *
 *  - every skipped crate@version must still exist in Cargo.lock, so a resolved
 *    duplicate forces its exception to be deleted;
 *  - every exception must carry a real, past ISO date and a reason;
 *  - the policy itself may not be softened into a blanket allow.
 */

const repository = new URL("../", import.meta.url);
const denyToml = await Bun.file(new URL("deny.toml", repository)).text();
const cargoLock = await Bun.file(new URL("Cargo.lock", repository)).text();
const ISO_DATE = /\d{4}-\d{2}-\d{2}/;
const TODAY = new Date().toISOString().slice(0, 10);

/** Lines of a top-level array such as `[bans] skip = [...]`. */
function arrayBody(source: string, key: string): string {
  const start = source.indexOf(`\n${key} = [`);
  if (start < 0) throw new Error(`deny.toml has no ${key} array`);
  const open = source.indexOf("[", start);
  const close = source.indexOf("\n]", open);
  if (close < 0) throw new Error(`deny.toml ${key} array is not terminated`);
  return source.slice(open + 1, close);
}

interface Exception {
  readonly spec: string;
  readonly reason: string;
}

function exceptions(body: string): Exception[] {
  return [...body.matchAll(/\{\s*(?:crate|id)\s*=\s*"([^"]+)"\s*,\s*reason\s*=\s*"([^"]*)"\s*\}/g)].map(
    (match) => ({ spec: match[1]!, reason: match[2]! }),
  );
}

const lockedVersions = new Map<string, Set<string>>();
for (const entry of cargoLock.matchAll(/\[\[package\]\]\nname = "([^"]+)"\nversion = "([^"]+)"/g)) {
  const versions = lockedVersions.get(entry[1]!) ?? new Set<string>();
  versions.add(entry[2]!);
  lockedVersions.set(entry[1]!, versions);
}

const duplicateSkips = exceptions(arrayBody(denyToml, "skip"));
const advisoryIgnores = exceptions(arrayBody(denyToml, "ignore"));

describe("cargo-deny policy", () => {
  it("keeps every check blocking and every registry vetted", () => {
    expect(denyToml).toContain('multiple-versions = "deny"');
    expect(denyToml).toContain('yanked = "deny"');
    expect(denyToml).toContain('unknown-registry = "deny"');
    expect(denyToml).toContain('unknown-git = "deny"');
    expect(denyToml).toContain('allow-registry = ["https://github.com/rust-lang/crates.io-index"]');
    expect(denyToml).toContain("allow-git = []");
    expect(denyToml, "bans.allow is a blanket exemption; use a dated skip instead").toContain("allow = []");
    expect(denyToml).not.toContain('multiple-versions = "allow"');
    expect(denyToml).not.toContain('multiple-versions = "warn"');
    expect(denyToml).not.toContain("skip-tree = [\n");

    // The wildcard lint is only excusable for path dependencies while the crates
    // that declare them are unpublishable. `Cargo.toml` marks the workspace
    // `publish = false`, so this gate has no reason to be softer than the rest.
    expect(denyToml, "the wildcard lint must stay blocking").toContain('wildcards = "deny"');
    expect(denyToml).not.toContain('wildcards = "warn"');
    expect(denyToml).not.toContain('wildcards = "allow"');
  });

  it("allows only permissive licences", () => {
    const allowed = arrayBody(denyToml, "allow");
    for (const forbidden of ["GPL-2.0", "GPL-3.0", "AGPL", "LGPL", "SSPL", "BUSL"]) {
      expect(allowed, `${forbidden} must not be allow-listed`).not.toContain(forbidden);
    }
    expect(allowed).toContain("Apache-2.0");
    expect(allowed).toContain("MIT");
  });

  it("dates and justifies every duplicate-version exception", () => {
    expect(duplicateSkips.length, "deny.toml records no duplicate exceptions").toBeGreaterThan(0);
    for (const entry of duplicateSkips) {
      expect(entry.spec, `${entry.spec} must pin an exact crate@version`).toMatch(/^[^@]+@[^@]+$/);
      const date = ISO_DATE.exec(entry.reason)?.[0];
      expect(date, `${entry.spec}: reason must carry an ISO review date`).toBeDefined();
      expect(date! <= TODAY, `${entry.spec}: review date is in the future`).toBe(true);
      expect(
        entry.reason.replace(ISO_DATE, "").length,
        `${entry.spec}: reason must say why it cannot move`,
      ).toBeGreaterThan(30);
    }
  });

  it("keeps no duplicate exception alive after the duplicate is gone", () => {
    const stale: string[] = [];
    for (const entry of duplicateSkips) {
      const at = entry.spec.lastIndexOf("@");
      const name = entry.spec.slice(0, at);
      const version = entry.spec.slice(at + 1);
      const versions = lockedVersions.get(name);
      if (!versions?.has(version)) {
        stale.push(`${entry.spec} is not in Cargo.lock`);
      } else if (versions.size < 2) {
        stale.push(`${name} no longer resolves to more than one version`);
      }
    }
    expect(stale, "delete these entries from deny.toml; the exception outlived the duplicate").toEqual([]);
  });

  it("never lets the exception list grow past the 2026-09-02 baseline", () => {
    expect(duplicateSkips.length).toBeLessThanOrEqual(27);
    expect(advisoryIgnores.length).toBeLessThanOrEqual(1);
  });

  it("dates and justifies every ignored advisory", () => {
    for (const entry of advisoryIgnores) {
      expect(entry.spec).toMatch(/^RUSTSEC-\d{4}-\d{4}$/);
      const date = ISO_DATE.exec(entry.reason)?.[0];
      expect(date, `${entry.spec}: reason must carry an ISO review date`).toBeDefined();
      expect(date! <= TODAY, `${entry.spec}: review date is in the future`).toBe(true);
      expect(entry.reason).toContain("wasmtime");
    }
  });
});

describe("dependency declarations", () => {
  it("declares an explicit version for every registry dependency", async () => {
    // cargo-deny only inspects the resolved graph; this asserts the same property
    // over every dependency table that exists, including the root
    // `[workspace.dependencies]` table that members now inherit from. Without the
    // root in this scan, centralising a dependency would move it out of the gate.
    const manifests = [
      "Cargo.toml",
      ...new Bun.Glob("workers/*/Cargo.toml").scanSync(repository.pathname),
      ...new Bun.Glob("crates/*/Cargo.toml").scanSync(repository.pathname),
    ].sort();
    expect(manifests.length).toBeGreaterThan(60);

    const offenders: string[] = [];
    for (const manifest of manifests) {
      const parsed = Bun.TOML.parse(
        await Bun.file(new URL(manifest, repository)).text(),
      ) as Record<string, unknown>;
      const workspace = (parsed.workspace ?? {}) as Record<string, unknown>;
      const tables: Array<[string, unknown]> = [
        ["dependencies", parsed.dependencies],
        ["dev-dependencies", parsed["dev-dependencies"]],
        ["build-dependencies", parsed["build-dependencies"]],
        ["workspace.dependencies", workspace.dependencies],
      ];
      for (const [table, dependencies] of tables) {
        if (typeof dependencies !== "object" || dependencies === null) continue;
        for (const [name, spec] of Object.entries(dependencies as Record<string, unknown>)) {
          const where = `${manifest} [${table}]`;
          if (typeof spec === "string") {
            if (spec.trim().length === 0 || spec.includes("*")) offenders.push(`${where}: ${name} = "${spec}"`);
            continue;
          }
          const detail = spec as Record<string, unknown>;
          const version = detail.version;
          if (typeof version === "string") {
            if (version.includes("*")) offenders.push(`${where}: ${name} version "${version}"`);
            continue;
          }
          if (typeof detail.path === "string" || detail.workspace === true) continue;
          offenders.push(`${where}: ${name} has no version, path or workspace inheritance`);
        }
      }
    }
    expect(offenders).toEqual([]);
  });
});
