import { describe, expect, it } from "bun:test";
import { existsSync } from "node:fs";
import { join } from "node:path";

/**
 * `plugin/.claude-plugin/plugin.json` still shipped the upstream identity after
 * the fork: author `iii-hq`, repository `iii-hq/agentos`, and "25 LLM providers"
 * for a router that declares 11. A plugin manifest is the first thing an
 * installer reads, so it must name this repository and count what this
 * repository has.
 */

const repository = new URL("../", import.meta.url);
const manifest = await Bun.file(new URL("plugin/.claude-plugin/plugin.json", repository)).json();
const ci = await Bun.file(new URL(".github/workflows/ci.yml", repository)).text();

const slug = /github\.repository == '([^']+)'/.exec(ci)?.[1];

describe("claude plugin manifest", () => {
  it("names this repository, not the upstream one", () => {
    expect(slug).toBeDefined();
    expect(manifest.repository).toBe(`https://github.com/${slug}`);
    expect(manifest.repository).not.toContain("iii-hq/agentos");
  });

  it("credits the owner of this fork", () => {
    expect(manifest.author.name).toBe("UnitB");
    expect(manifest.author.url).toBe(`https://github.com/${slug!.split("/")[0]}`);
  });

  it("keeps the licence and version aligned with the workspace", async () => {
    const cargo = await Bun.file(new URL("Cargo.toml", repository)).text();
    const version = /^version = "([^"]+)"$/m.exec(cargo)?.[1];
    expect(manifest.version).toBe(version);
    expect(manifest.license).toBe("Apache-2.0");
  });

  it("still declares every capability directory it advertises", () => {
    const missing: string[] = [];
    for (const relative of Object.values(manifest.main) as string[]) {
      const path = relative.replace(/^\.\//, "plugin/");
      if (!existsSync(join(repository.pathname, path))) missing.push(path);
    }
    expect(missing, "plugin manifest points at paths that do not exist").toEqual([]);
  });
});
