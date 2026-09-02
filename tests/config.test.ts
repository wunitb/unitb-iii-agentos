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

  // Replacement requested by sec-perimeter (2026-09-02): `shell` puts
  // shell::exec / shell::fs::* / coder::* on an unauthenticated bus and the
  // fs jail constrains only the shell::fs::* half, and console v1.9.16 has no
  // bind-host key so it cannot be confined to loopback. Neither is booted by
  // default any more, so the config.yaml half of the old assertion becomes the
  // stronger claim: they must be absent.
  it("does not boot the shell, console or harness worker by default", async () => {
    const root = await Bun.file(new URL("config.yaml", repository)).text();
    expect(root, "shell is an arbitrary-command sink on the unauthenticated bus").not.toMatch(
      /^\s*-\s*name:\s*shell\s*$/m,
    );
    expect(root, "console v1.9.16 cannot be bound to loopback").not.toMatch(
      /^\s*-\s*name:\s*console\s*$/m,
    );
    expect(
      root,
      "harness v1.8.8-rc.3 registers harness::spawn, harness::function::trigger and " +
        "harness::filesystem::grant/revoke on the unauthenticated bus",
    ).not.toMatch(/^\s*-\s*name:\s*harness\s*$/m);
  });

  // The bus carries every AgentOS function and has no authentication of its own.
  // `iii-worker-manager` is mandatory: when config.yaml omits it the engine appends
  // it with WorkerManagerConfig::default(), whose host is 0.0.0.0 — which is how the
  // bus became reachable from the LAN. Losing this entry is silent, so it is asserted.
  it("pins the engine bus to loopback", async () => {
    const root = await Bun.file(new URL("config.yaml", repository)).text();
    const entry = /^\s*-\s*name:\s*iii-worker-manager\s*$/m.exec(root);
    expect(entry, "config.yaml does not declare iii-worker-manager; the bus would bind 0.0.0.0").not.toBeNull();

    const rest = root.slice(entry!.index + entry![0].length);
    const nextEntry = /^\s*-\s*name:/m.exec(rest);
    const block = nextEntry ? rest.slice(0, nextEntry.index) : rest;
    expect(block, "iii-worker-manager must pin host: 127.0.0.1").toMatch(/^\s*host:\s*127\.0\.0\.1\s*$/m);
    expect(block, "the bus must not be bound to all interfaces").not.toMatch(/0\.0\.0\.0/);
  });

  it("keeps the shell worker confined to the checkout if it is opted in", async () => {
    const shell = await Bun.file(new URL("config/shell.yaml", repository)).text();
    expect(shell).toContain("host_roots:");
    expect(shell).toContain("${III_COMPOSE_DIR:.}");
    expect(shell).toContain("allow_unjailed: false");
    expect(shell).not.toContain("allow_unjailed: true");
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
