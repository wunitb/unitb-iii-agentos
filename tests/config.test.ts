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
  return [...source.matchAll(/^\s*-\s+name:\s*([^\s]+)/gm)].map(
    (match) => match[1],
  );
}

function engine023Name(name: string): string {
  return deprecatedAliases.get(name) ?? name;
}

describe("iii 0.23 OCI engine config", () => {
  it("boots without deprecated/canonical worker collisions", async () => {
    const source = await Bun.file(new URL("config.yaml", repository)).text();
    const workers = configuredWorkers(source);
    const canonical = workers.map(engine023Name);

    const compose = Bun.YAML.parse(await Bun.file(new URL("worker-compose.yaml", repository)).text()) as { containers: Record<string, { version: string; worker: string }>; engine?: unknown; namespace: string };
    const lock = Bun.YAML.parse(await Bun.file(new URL("iii.lock", repository)).text()) as { workers: Record<string, { version: string }> };
    expect(compose.engine).toBeUndefined();
    expect(compose.namespace).toBe("default");
    for (const name of ["queue", "state", "cron", "llm-router", "context-manager", "iii-directory", "provider-anthropic", "provider-openai", "provider-openai-codex", "session-manager"]) {
      expect(workers).not.toContain(name);
      expect(compose.containers[name].worker).toBe(`package://api.workers.iii.dev/${name}`);
      expect(compose.containers[name].version).toBe(String(lock.workers[name].version));
    }
    for (const [name, version, config] of [["http", "0.21.9", "iii-http"], ["pubsub", "0.21.5", "iii-pubsub"]]) {
      expect(compose.containers[name].worker).toBe(`package://api.workers.iii.dev/${name}`);
      expect(compose.containers[name].version).toBe(version);
      expect((compose.containers[name] as { config_name?: string }).config_name).toBe(config);
    }
    expect(Object.keys(compose.containers)).toHaveLength(12);
    for (const worker of Object.values(compose.containers)) {
      expect((worker as { working_dir?: string }).working_dir).toBe(".");
    }
    for (const name of ["shell", "console", "harness", "iii-bridge"]) expect(compose.containers[name]).toBeUndefined();
    for (const name of workers) expect(["configuration", "iii-worker-manager#raw", "iii-worker-manager", "iii-stream", "iii-http-functions", "iii-sandbox"]).toContain(name);
    expect(workers.filter((name) => deprecatedAliases.has(name))).toEqual([]);
    expect(new Set(canonical).size).toBe(canonical.length);
  });

  it("declares only scoped provider credentials for Compose children", async () => {
    const compose = Bun.YAML.parse(await Bun.file(new URL("worker-compose.yaml", repository)).text()) as { containers: Record<string, { environment: Record<string, string> }> };
    for (const [name, worker] of Object.entries(compose.containers)) {
      expect(worker.environment.AGENTOS_API_KEY).toBeUndefined();
      expect(worker.environment.III_URL).toBeUndefined();
      expect(worker.environment.TOKIO_WORKER_THREADS).toBe("${TOKIO_WORKER_THREADS:-2}");
      expect(worker.environment.III_DISABLE_TRACE_PAYLOADS).toBeDefined();
      if (!["llm-router", "provider-anthropic"].includes(name)) expect(worker.environment.ANTHROPIC_API_KEY).toBeUndefined();
      if (!["llm-router", "provider-openai"].includes(name)) expect(worker.environment.OPENAI_API_KEY).toBeUndefined();
      if (name !== "provider-openai-codex") expect(worker.environment.CODEX_HOME).toBeUndefined();
    }
    expect(compose.containers["provider-anthropic"].environment.ANTHROPIC_API_KEY).toBe("${ANTHROPIC_API_KEY:-}");
    expect(compose.containers["provider-openai"].environment.OPENAI_API_KEY).toBe("${OPENAI_API_KEY:-}");
    expect(compose.containers["provider-openai-codex"].environment.CODEX_HOME).toBe("${CODEX_HOME:-}");
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

  it("allows only the fixed container-private raw manager plus the fully gated edge", async () => {
    const root = Bun.YAML.parse(await Bun.file(new URL("config.yaml", repository)).text()) as { workers: { name: string; config?: Record<string, unknown> }[] };
    const managers = root.workers.filter((entry) => entry.name.split("#")[0] === "iii-worker-manager");
    expect(managers).toHaveLength(2);
    expect(managers.find((entry) => entry.name === "iii-worker-manager#raw")?.config).toEqual({ host: "127.0.0.1", port: 49129 });
    expect(managers.find((entry) => entry.name === "iii-worker-manager")?.config).toEqual({
      host: "0.0.0.0", port: 49134,
      rbac: {
        auth_function_id: "agentos::bus_auth",
        on_function_registration_function_id: "agentos::bus_on_register",
        on_trigger_registration_function_id: "agentos::bus_on_trigger",
        on_trigger_type_registration_function_id: "agentos::bus_on_trigger_type",
        expose_functions: ['match("*")'],
      },
    });
    const http = Bun.YAML.parse(await Bun.file(new URL("config/iii-http.yaml", repository)).text()) as { value: { host: string } };
    const stream = Bun.YAML.parse(await Bun.file(new URL("config/iii-stream.yaml", repository)).text()) as { value: { host: string } };
    expect(http.value.host).toBe("0.0.0.0");
    expect(stream.value.host).toBe("127.0.0.1");
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

it("uses the pinned registry cron worker's local lock adapter in a single OCI runtime", async () => {
  const cron = Bun.YAML.parse(await Bun.file(new URL("config/cron.yaml", repository)).text()) as { value: { adapter: { name: string } } };
  expect(cron.value.adapter.name).toBe("local");
});
