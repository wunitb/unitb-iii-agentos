import { describe, expect, it } from "bun:test";

const repository = new URL("../", import.meta.url);
const deprecatedAliases = new Map([
  ["iii-queue", "queue"],
  ["iii-state", "state"],
  ["iii-cron", "cron"],
]);

const excludedSourcePrefixes = [
  "target/",
  "node_modules/",
  "website/node_modules/",
  "website/dist/",
  "dist/",
  "coverage/",
  ".upstream-iii/",
] as const;

function isRepositorySource(path: string): boolean {
  return !excludedSourcePrefixes.some((prefix) => path.startsWith(prefix));
}

function configuredWorkers(source: string): string[] {
  return [...source.matchAll(/^\s*-\s+name:\s*([^\s#]+)/gm)].map(
    (match) => match[1],
  );
}

function engine0221Name(name: string): string {
  return deprecatedAliases.get(name) ?? name;
}

describe("iii 0.22.1 engine config", () => {
  it("boots without deprecated/canonical worker collisions", async () => {
    const source = await Bun.file(new URL("config.yaml", repository)).text();
    const workers = configuredWorkers(source);
    const canonical = workers.map(engine0221Name);

    expect(workers).toContain("queue");
    expect(workers).toContain("state");
    expect(workers).toContain("cron");
    expect(workers.filter((name) => deprecatedAliases.has(name))).toEqual([]);
    expect(new Set(canonical).size).toBe(canonical.length);
  });

  it("stores canonical configuration under canonical worker ids", async () => {
    for (const worker of ["queue", "state", "cron"]) {
      const source = await Bun.file(
        new URL(`config/${worker}.yaml`, repository),
      ).text();
      expect(source).toMatch(new RegExp(`^id: ${worker}$`, "m"));
      expect(await Bun.file(new URL(`config/iii-${worker}.yaml`, repository)).exists()).toBe(
        false,
      );
    }
  });

  it("keeps the shell worker confined to the checkout", async () => {
    for (const path of ["config.yaml", "config/shell.yaml"]) {
      const source = await Bun.file(new URL(path, repository)).text();
      expect(source, path).toContain("host_roots:");
      expect(source, path).toContain("${III_COMPOSE_DIR:.}");
      expect(source, path).toContain("allow_unjailed: false");
      expect(source, path).not.toContain("allow_unjailed: true");
    }
  });

  it("has no deprecated worker references outside this regression test", async () => {
    const glob = new Bun.Glob("**/*.{yaml,yml,rs,ts,tsx,js,jsx,md,sh}");
    for await (const path of glob.scan({ cwd: repository.pathname })) {
      if (path === "tests/config.test.ts" || !isRepositorySource(path)) {
        continue;
        }
      const source = await Bun.file(new URL(path, repository)).text();
      for (const deprecated of deprecatedAliases.keys()) {
        expect(source, `${path} still references ${deprecated}`).not.toContain(
          deprecated,
        );
      }
    }
  });
});
