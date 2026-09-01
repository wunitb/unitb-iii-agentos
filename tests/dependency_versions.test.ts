import { describe, expect, it } from "bun:test";

const repository = new URL("../", import.meta.url);

async function sourceFiles(pattern: string): Promise<string[]> {
  const files: string[] = [];
  for await (const path of new Bun.Glob(pattern).scan({ cwd: repository.pathname })) {
    if (path.startsWith("target/") || path.startsWith("node_modules/")) continue;
    files.push(path);
  }
  return files;
}

function packageVersions(lockfile: string, packageName: string): string[] {
  const versions: string[] = [];
  const blocks = lockfile.split("[[package]]");
  for (const block of blocks) {
    if (!new RegExp(`\\n?name = "${packageName}"(?:\\n|$)`).test(block)) continue;
    const version = block.match(/\nversion = "([^"]+)"/);
    if (version) versions.push(version[1]);
  }
  return versions.sort();
}

async function directDependencyVersions(dependency: string): Promise<Array<[string, string]>> {
  const versions: Array<[string, string]> = [];
  const pattern = new RegExp(`^${dependency}\\s*=\\s*"([^"]+)"`, "m");
  for (const path of await sourceFiles("**/Cargo.toml")) {
    const source = await Bun.file(new URL(path, repository)).text();
    const match = source.match(pattern);
    if (match) versions.push([path, match[1]]);
  }
  return versions;
}

describe("Rust dependency compatibility contract", () => {
  it("keeps direct base64 users on 0.23 while accepting the transitive 0.22 line", async () => {
    const direct = await directDependencyVersions("base64");
    expect(direct.length, "expected direct base64 declarations").toBeGreaterThan(0);
    expect(
      direct.filter(([, version]) => !version.startsWith("0.23")),
      "direct base64 dependencies must use the 0.23 API line",
    ).toEqual([]);

    const lockfile = await Bun.file(new URL("Cargo.lock", repository)).text();
    const locked = packageVersions(lockfile, "base64");
    expect(locked, "Cargo.lock must retain the transitive 0.22 line used by upstream crates").toContain(
      "0.22.1",
    );
    expect(
      locked.some((version) => version.startsWith("0.23.")),
      "Cargo.lock must also resolve the direct 0.23 line",
    ).toBe(true);
  });

  it("holds Wasmtime on the Rust 1.90-compatible 36.x line", async () => {
    const workspaceManifest = await Bun.file(new URL("Cargo.toml", repository)).text();
    expect(workspaceManifest).toMatch(/rust-version\s*=\s*"1\.90"/);

    const direct = await directDependencyVersions("wasmtime");
    expect(direct).toHaveLength(1);
    expect(
      direct[0][1].startsWith("36."),
      "Wasmtime must stay on 36.x until the workspace Rust 1.90 contract is intentionally raised",
    ).toBe(true);

    const lockfile = await Bun.file(new URL("Cargo.lock", repository)).text();
    const locked = packageVersions(lockfile, "wasmtime");
    expect(locked).toHaveLength(1);
    expect(
      locked[0].startsWith("36."),
      "locked Wasmtime must remain compiler-compatible with Rust 1.90",
    ).toBe(true);

    for (const workflow of [".github/workflows/ci.yml", ".github/workflows/release.yml"]) {
      const source = await Bun.file(new URL(workflow, repository)).text();
      expect(source, `${workflow} must use the workspace Rust contract`).toContain('toolchain: "1.90"');
    }
  });
});
