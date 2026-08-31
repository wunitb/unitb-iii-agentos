import { describe, expect, it } from "bun:test";

const repository = new URL("../", import.meta.url);

async function canonicalVersion(): Promise<string> {
  return (await Bun.file(new URL(".iii-version", repository)).text()).trim();
}

async function sourceFiles(pattern: string): Promise<string[]> {
  const files: string[] = [];
  const glob = new Bun.Glob(pattern);
  for await (const path of glob.scan({ cwd: repository.pathname })) {
    if (
      path.startsWith("target/") ||
      path.startsWith("node_modules/") ||
      path.startsWith("website/node_modules/") ||
      path.startsWith("website/dist/") ||
      path.startsWith(".upstream-iii/")
    ) {
      continue;
    }
    files.push(path);
  }
  return files;
}

describe("iii stable version contract", () => {
  it("uses a stable semantic version as the canonical pin", async () => {
    expect(await canonicalVersion()).toMatch(/^\d+\.\d+\.\d+$/);
  });

  it("keeps every Rust iii-sdk dependency aligned", async () => {
    const version = await canonicalVersion();
    const failures: string[] = [];
    for (const path of await sourceFiles("**/Cargo.toml")) {
      const source = await Bun.file(new URL(path, repository)).text();
      for (const match of source.matchAll(/iii-sdk\s*=\s*"=([^"]+)"/g)) {
        if (match[1] !== version) failures.push(`${path}: ${match[1]}`);
      }
    }
    expect(failures).toEqual([]);
  });

  it("keeps Node and Python SDK pins aligned", async () => {
    const version = await canonicalVersion();
    const packageJson = await Bun.file(new URL("package.json", repository)).json();
    expect(packageJson.dependencies["iii-sdk"]).toBe(version);

    const pyproject = await Bun.file(
      new URL("workers/embedding/pyproject.toml", repository),
    ).text();
    const worker = await Bun.file(
      new URL("workers/embedding/iii.worker.yaml", repository),
    ).text();
    expect(pyproject).toContain(`iii-sdk==${version}`);
    expect(worker).toContain(`iii-sdk==${version}`);
  });

  it("makes installers and current docs consume the canonical pin", async () => {
    const version = await canonicalVersion();
    for (const path of ["scripts/install-iii.sh", "scripts/install.sh"]) {
      const source = await Bun.file(new URL(path, repository)).text();
      expect(source, path).toContain(".iii-version");
      expect(source, path).toContain("iii-worker");
    }
    for (const path of ["README.md", "AGENTS.md", "ARCHITECTURE.md"]) {
      const source = await Bun.file(new URL(path, repository)).text();
      expect(source, path).toContain(`v${version}`);
    }
  });
});
