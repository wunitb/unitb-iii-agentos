import { describe, expect, it } from "bun:test";

/** Source contract only; real boot/registry/queue proof runs in the required OCI CI lane. */

const repository = new URL("../", import.meta.url);
const expectedVersion = (
  await Bun.file(new URL(".iii-version", repository)).text()
).trim();
// Official iii v0.22.1 artifacts report "queue" on macOS and
// "queue-engine" on Linux.
describe(`iii ${expectedVersion} OCI boot contract`, () => {
  it("keeps the private bootstrap manager separate from the authenticated edge", async () => {
    const config = Bun.YAML.parse(await Bun.file(new URL("config.yaml", repository)).text()) as { workers: { name: string; config?: { host?: string; port?: number; rbac?: Record<string, unknown> } }[] };
    const managers = config.workers.filter((worker) => worker.name.startsWith("iii-worker-manager"));
    expect(managers.map((worker) => worker.name).sort()).toEqual(["iii-worker-manager", "iii-worker-manager#raw"]);
    expect(managers.find((worker) => worker.name.endsWith("#raw"))?.config).toEqual({ host: "127.0.0.1", port: 49129 });
    const edge = managers.find((worker) => worker.name === "iii-worker-manager")?.config;
    expect(edge?.host).toBe("0.0.0.0");
    expect(edge?.port).toBe(49134);
    expect(Object.keys(edge?.rbac ?? {}).filter((key) => key.endsWith("function_id"))).toHaveLength(4);
  });

  it("pins queue and state as Compose primitives instead of duplicate native workers", async () => {
    const compose = Bun.YAML.parse(await Bun.file(new URL("worker-compose.yaml", repository)).text()) as { engine?: unknown; containers: Record<string, { worker: string; version: string; config_name: string }> };
    expect(compose.engine).toBeUndefined();
    for (const name of ["queue", "state", "cron"]) {
      expect(compose.containers[name].worker).toBe(`package://api.workers.iii.dev/${name}`);
      expect(compose.containers[name].version).toMatch(/^\d+\.\d+\.\d+$/);
      expect(compose.containers[name].config_name).toBe(name);
    }
  });

  it("requires actual OCI acceptance and its queue provider evidence in CI", async () => {
    const ci = await Bun.file(new URL(".github/workflows/ci.yml", repository)).text();
    const smoke = await Bun.file(new URL("scripts/oci-smoke.py", repository)).text();
    const manifest = await Bun.file(new URL("package.json", repository)).json();
    expect(manifest.scripts["test:oci"]).toBe("python3 scripts/oci-smoke.py");
    expect(ci).toContain('python3 scripts/oci-smoke.py --report "$report"');
    expect(ci).toContain('bun scripts/assert-oci-results.ts "$report"');
    expect(smoke).toContain("engine::queue::enqueue");
    expect(smoke).toContain("configuration::get");
    expect(smoke).not.toContain("reapExternalWorkers");
  });
});
