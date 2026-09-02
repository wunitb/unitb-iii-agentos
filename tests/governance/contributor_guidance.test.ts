import { describe, expect, it } from "bun:test";

/**
 * `AGENTS.md` still carried a "Managed UNITB OMPAX fleet" section on 2026-09-02,
 * ten days after that fleet was withdrawn (2026-08-22), and its build table still
 * told contributors to run a release-profile test suite behind `rustup run 1.90`.
 * Contributor guidance that describes a dead process, or a different bar from the
 * one CI enforces, is worse than no guidance.
 */

const repository = new URL("../../", import.meta.url);
const guidance = await Bun.file(new URL("AGENTS.md", repository)).text();
const ci = await Bun.file(new URL(".github/workflows/ci.yml", repository)).text();
const release = await Bun.file(new URL(".github/workflows/release.yml", repository)).text();

describe("contributor guidance", () => {
  it("no longer governs the withdrawn OMPAX fleet", () => {
    expect(guidance).not.toContain("## Managed UNITB OMPAX fleet");
    expect(guidance).not.toContain("The managed Planner is read-only");
    expect(guidance).not.toContain("Publication uses `fleet_handoff`");
    // It may name the withdrawal, so readers of stale documents can recognise it.
    expect(guidance).toContain("withdrawn on 2026-08-22");
  });

  it("states the same Rust bar CI enforces, with no rustup prefix", () => {
    expect(guidance).not.toContain("rustup run 1.90");

    const gates = [
      "cargo fmt --all -- --check",
      "cargo clippy --workspace --all-targets --locked -- -D warnings",
      "cargo test --workspace --locked",
    ];
    for (const gate of gates) {
      expect(ci.includes(`run: ${gate}`), `ci.yml does not run \`${gate}\``).toBe(true);
    }
    expect(guidance).toContain("cargo fmt --all -- --check");
    expect(guidance).toContain("cargo clippy --workspace --all-targets --locked -- -D warnings");
    expect(guidance, "AGENTS.md must not advertise a release-profile test run CI no longer does").not.toContain(
      "cargo test --workspace --release",
    );
    expect(guidance).toContain("cargo deny check");
  });

  it("names the Node gates CI actually runs", () => {
    for (const script of ["test:unit", "test:governance", "counts:check"]) {
      expect(ci).toContain(`bun run ${script}`);
      expect(guidance, `AGENTS.md does not mention ${script}`).toContain(script);
    }
  });

  it("counts the release targets the release workflow builds", () => {
    const matrix = release.slice(release.indexOf("  build:"), release.indexOf("  validate:"));
    const targets = [...matrix.matchAll(/^ {12}arch: \S+$/gm)];
    expect(targets.length).toBe(3);
    expect(guidance).toContain("three supported targets");
    expect(guidance).not.toContain("four supported targets");
  });
});
