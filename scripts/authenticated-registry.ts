import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const GENERATED_KEY = /^[0-9a-f]{64}$/;

/** Read the one key generated into the isolated runtime without mutating this process. */
export function readGeneratedApiKey(source: string): string {
  const assignments = source.split(/\r?\n/).flatMap((line) => {
    const trimmed = line.trimStart().replace(/^export\s+/, "");
    const separator = trimmed.indexOf("=");
    if (separator < 0 || trimmed.slice(0, separator).trim() !== "AGENTOS_API_KEY") return [];
    return [trimmed.slice(separator + 1).trim()];
  });
  if (assignments.length !== 1 || !GENERATED_KEY.test(assignments[0] ?? "")) {
    throw new Error("expected exactly one generated AGENTOS_API_KEY in the isolated runtime .env");
  }
  return assignments[0];
}

export async function authenticatedRegistry(dotenvPath: string, wsUrl: string): Promise<unknown> {
  const apiKey = readGeneratedApiKey(await readFile(dotenvPath, "utf8"));
  const { registerWorker } = await import("iii-sdk");
  const client = registerWorker(wsUrl, {
    workerName: "agentos-boot-smoke-registry",
    enableMetricsReporting: false,
    invocationTimeoutMs: 5_000,
    otel: { enabled: false },
    headers: { Authorization: `Bearer ${apiKey}` },
  });
  try {
    return await client.trigger({
      function_id: "engine::functions::list",
      payload: {},
      timeoutMs: 5_000,
    });
  } finally {
    await client.shutdown();
  }
}

async function main(): Promise<void> {
  const dotenvPath = process.argv[2];
  if (!dotenvPath) throw new Error("isolated runtime .env path is required");
  const registry = await authenticatedRegistry(
    dotenvPath,
    process.env.III_URL ?? "ws://127.0.0.1:49134",
  );
  process.stdout.write(`${JSON.stringify(registry)}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(() => {
    // Do not print the SDK error object: it may retain handshake headers.
    console.error("authenticated registry query failed");
    process.exitCode = 1;
  });
}
