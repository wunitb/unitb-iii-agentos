import { describe, expect, it } from "bun:test";

/**
 * Before `rust-toolchain.toml` existed, CI pinned 1.90 in two workflow files and
 * a contributor's default `rustup stable` ran a different linter: on 2026-09-02
 * `cargo clippy -- -D warnings` was clean on 1.90 and reported 13 diagnostics on
 * stable 1.97.1. A gate that only the gate can reproduce trains people to ignore
 * it. This suite keeps the four declarations of the toolchain identical.
 */

const repository = new URL("../", import.meta.url);

async function text(path: string): Promise<string> {
  return await Bun.file(new URL(path, repository)).text();
}

function capture(source: string, pattern: RegExp, what: string): string {
  const match = pattern.exec(source);
  if (!match?.[1]) throw new Error(`could not read ${what}`);
  return match[1];
}

describe("rust toolchain pin", () => {
  it("declares one channel in rust-toolchain.toml with the linting components", async () => {
    const toolchain = await text("rust-toolchain.toml");

    expect(toolchain).toContain("[toolchain]");
    expect(capture(toolchain, /^channel = "([^"]+)"$/m, "rust-toolchain.toml channel")).toBe("1.90");
    expect(toolchain).toContain("clippy");
    expect(toolchain).toContain("rustfmt");
  });

  it("agrees with Cargo.toml rust-version and both workflows", async () => {
    const channel = capture(
      await text("rust-toolchain.toml"),
      /^channel = "([^"]+)"$/m,
      "rust-toolchain.toml channel",
    );

    const msrv = capture(
      await text("Cargo.toml"),
      /^rust-version = "([^"]+)"$/m,
      "Cargo.toml rust-version",
    );
    expect(msrv).toBe(channel);

    for (const workflow of [".github/workflows/ci.yml", ".github/workflows/release.yml"]) {
      const source = await text(workflow);
      const pinned = [...source.matchAll(/^ +toolchain: "([^"]+)"$/gm)].map((match) => match[1]);
      expect(pinned.length, `${workflow} pins no toolchain`).toBeGreaterThan(0);
      for (const value of pinned) {
        expect(value, `${workflow} pins ${value}, not ${channel}`).toBe(channel);
      }
    }

    const ci = await text(".github/workflows/ci.yml");
    expect(capture(ci, /AGENTOS_RUST_TOOLCHAIN: "([^"]+)"/, "ci.yml AGENTOS_RUST_TOOLCHAIN")).toBe(channel);
  });

  it("installs clippy and rustfmt wherever it installs the toolchain", async () => {
    for (const workflow of [".github/workflows/ci.yml", ".github/workflows/release.yml"]) {
      const source = await text(workflow);
      const steps = source.split("dtolnay/rust-toolchain@").slice(1);
      expect(steps.length, `${workflow} installs no toolchain`).toBeGreaterThan(0);
      for (const step of steps) {
        const head = step.slice(0, 400);
        expect(head, `${workflow} installs a toolchain without rustfmt`).toContain("rustfmt");
        expect(head, `${workflow} installs a toolchain without clippy`).toContain("clippy");
      }
    }
  });
});
