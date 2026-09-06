import { readFile, writeFile } from "node:fs/promises";
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

async function withoutSdkOutput<T>(operation: () => Promise<T>): Promise<T> {
  const stdoutWrite = process.stdout.write;
  const stderrWrite = process.stderr.write;
  const consoleMethods = {
    debug: console.debug,
    error: console.error,
    info: console.info,
    log: console.log,
    warn: console.warn,
  };
  const discard = (() => true) as typeof process.stdout.write;
  process.stdout.write = discard;
  process.stderr.write = discard as typeof process.stderr.write;
  console.debug = () => {};
  console.error = () => {};
  console.info = () => {};
  console.log = () => {};
  console.warn = () => {};
  try {
    return await operation();
  } finally {
    process.stdout.write = stdoutWrite;
    process.stderr.write = stderrWrite;
    console.debug = consoleMethods.debug;
    console.error = consoleMethods.error;
    console.info = consoleMethods.info;
    console.log = consoleMethods.log;
    console.warn = consoleMethods.warn;
  }
}

export async function authenticatedRegistry(dotenvPath: string, wsUrl: string): Promise<unknown> {
  const apiKey = readGeneratedApiKey(await readFile(dotenvPath, "utf8"));
  return withoutSdkOutput(async () => {
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
  });
}

async function main(): Promise<void> {
  const dotenvPath = process.argv[2];
  const outputPath = process.argv[3];
  if (!dotenvPath || !outputPath) {
    throw new Error("isolated runtime .env and output paths are required");
  }
  const registry = await authenticatedRegistry(
    dotenvPath,
    process.env.III_URL ?? "ws://127.0.0.1:49134",
  );
  await writeFile(outputPath, `${JSON.stringify(registry)}\n`, {
    encoding: "utf8",
    flag: "wx",
    mode: 0o600,
  });
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(() => {
    // Do not print the SDK error object: it may retain handshake headers.
    console.error("authenticated registry query failed");
    process.exitCode = 1;
  });
}
