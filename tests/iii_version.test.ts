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

  /**
   * The scan above is satisfied by zero declarations, so on its own it stopped
   * proving anything once the pin moved into `[workspace.dependencies]`. The
   * invariant it is really protecting is "exactly one pinned iii-sdk version for
   * the whole workspace", which is a property of *where* the pin lives, not of
   * how a member spells it.
   */
  it("pins iii-sdk exactly once, in the workspace table every member inherits", async () => {
    const version = await canonicalVersion();
    const workspaceManifest = Bun.TOML.parse(
      await Bun.file(new URL("Cargo.toml", repository)).text(),
    ) as { workspace?: { members?: string[]; dependencies?: Record<string, unknown> } };

    expect(workspaceManifest.workspace?.dependencies?.["iii-sdk"]).toBe(`=${version}`);

    const declarations: string[] = [];
    const notInherited: string[] = [];
    for (const member of workspaceManifest.workspace?.members ?? []) {
      const manifest = `${member}/Cargo.toml`;
      const parsed = Bun.TOML.parse(await Bun.file(new URL(manifest, repository)).text()) as {
        dependencies?: Record<string, unknown>;
      };
      const spec = parsed.dependencies?.["iii-sdk"];
      if (spec === undefined) continue;
      if (typeof spec === "object" && spec !== null && (spec as { workspace?: boolean }).workspace === true) {
        continue;
      }
      declarations.push(manifest);
      notInherited.push(`${manifest}: ${JSON.stringify(spec)}`);
    }

    expect(
      notInherited,
      "these members pin iii-sdk themselves; a version bump would have to sweep every one of them",
    ).toEqual([]);
    expect(declarations).toEqual([]);
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
    const installer = await Bun.file(
      new URL("scripts/install-iii.sh", repository),
    ).text();
    expect(installer).toContain("binaries=(iii iii-worker iii-console)");
    expect(installer).toContain("binaries+=(iii-init)");
    expect(installer).toContain("skipping broken macOS release artifact");
    expect(installer).toContain("iii-hq/iii#2119");

    for (const path of ["README.md", "AGENTS.md", "ARCHITECTURE.md"]) {
      const source = await Bun.file(new URL(path, repository)).text();
      expect(source, path).toContain(`v${version}`);
    }
  });
});
