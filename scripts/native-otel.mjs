import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { once } from "node:events";
import { access, mkdir, mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises";
import { connect, createServer } from "node:net";
import { tmpdir } from "node:os";
import { delimiter, isAbsolute, join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { bounded } from "./fixtures/otel-support.mjs";

const execFileAsync = promisify(execFile);
const self = fileURLToPath(import.meta.url);
const clientFixture = fileURLToPath(new URL("./fixtures/native-otel-client.mjs", import.meta.url));

async function executable(name) {
  const candidates = isAbsolute(name) ? [name] : (process.env.PATH ?? "").split(delimiter).map((directory) => join(directory, name));
  for (const candidate of candidates) {
    try { await access(candidate, 1); return await realpath(candidate); } catch { /* try the next PATH entry */ }
  }
  throw new Error(`required executable not found: ${name}`);
}

async function listening(port) {
  return new Promise((resolve) => {
    const socket = connect({ host: "127.0.0.1", port });
    const finish = (ready) => { socket.destroy(); resolve(ready); };
    socket.once("connect", () => finish(true));
    socket.once("error", () => finish(false));
    socket.setTimeout(300, () => finish(false));
  });
}

async function availablePort() {
  const reservation = createServer();
  reservation.listen(0, "127.0.0.1");
  await once(reservation, "listening");
  const port = reservation.address().port;
  await new Promise((resolve) => reservation.close(resolve));
  return port;
}

async function runNative(port) {
  const engine = await executable(process.env.III_BIN ?? "iii");
  const pin = (await readFile(new URL("../.iii-version", import.meta.url), "utf8")).trim();
  const version = (await execFileAsync(engine, ["--version"])).stdout.trim();
  assert.equal(version, pin, "native acceptance requires the exact engine pin");
  const scratch = await mkdtemp(join(tmpdir(), "agentos-native-otel-"));
  const config = {
    workers: [
      { name: "iii-worker-manager", config: { host: "127.0.0.1", port } },
      { name: "configuration", config: { adapter: { name: "fs", config: { directory: "./config" } }, ttl_seconds: 0 } },
    ],
  };
  await mkdir(join(scratch, "config"));
  // iii 0.23 boots observability internally; its settings belong in the
  // configuration store, not an unsupported config.yaml worker entry.
  await writeFile(join(scratch, "config", "iii-observability.yaml"), JSON.stringify({
    id: "iii-observability",
    name: "Native OTEL acceptance",
    description: "Isolated SDK trace, metric and log ingestion acceptance",
    value: { enabled: true, exporter: "memory", metrics_enabled: true, metrics_exporter: "memory", logs_enabled: true, logs_console_output: false, sampling_ratio: 1, logs_sampling_ratio: 1 },
  }));
  await mkdir(join(scratch, ".iii"));
  await writeFile(join(scratch, ".iii", "telemetry_dev_optout"), "");
  await writeFile(join(scratch, "config.yaml"), JSON.stringify(config));
  let output = "";
  const child = spawn(engine, ["--no-update-check", "--config", "config.yaml"], {
    cwd: scratch,
    env: { PATH: process.env.PATH, HOME: scratch, XDG_CONFIG_HOME: scratch, XDG_CACHE_HOME: scratch, III_TELEMETRY_DEV: "true", IIIWORKER_DISABLE_BUILTIN_DAEMONS: "1" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout.on("data", (chunk) => { output = (output + chunk).slice(-16_384); });
  child.stderr.on("data", (chunk) => { output = (output + chunk).slice(-16_384); });
  const exited = once(child, "exit");
  const results = [];
  const cancellation = new AbortController();
  const cancel = () => cancellation.abort(new Error("native acceptance interrupted"));
  process.once("SIGINT", cancel);
  process.once("SIGTERM", cancel);
  try {
    const deadline = Date.now() + 15_000;
    while (!(await listening(port))) {
      cancellation.signal.throwIfAborted();
      assert.equal(child.exitCode, null, `native engine exited: ${output}`);
      assert.ok(Date.now() < deadline, `native engine did not become ready: ${output}`);
      await delay(50);
    }
    const endpoint = process.env.PORTLESS_URL ? process.env.PORTLESS_URL.replace(/^https:/, "wss:").replace(/^http:/, "ws:") : `ws://127.0.0.1:${port}`;
    for (const runtime of ["node", "bun"]) {
      for (const format of ["esm", "cjs"]) {
        const binary = await executable(runtime === "node" ? (process.env.AGENTOS_OTEL_NODE_BIN ?? process.execPath) : (process.env.AGENTOS_OTEL_BUN_BIN ?? "bun"));
        const args = runtime === "bun" ? ["--no-env-file", clientFixture, format] : [clientFixture, format];
        const env = { PATH: process.env.PATH, III_URL: endpoint };
        if (process.env.NODE_EXTRA_CA_CERTS) env.NODE_EXTRA_CA_CERTS = process.env.NODE_EXTRA_CA_CERTS;
        const result = await execFileAsync(binary, args, { cwd: scratch, env, signal: cancellation.signal, timeout: 25_000, maxBuffer: 1024 * 1024 });
        assert.equal(result.stderr, "", result.stderr);
        const summary = result.stdout.split("\n").find((line) => line.startsWith("AGENTOS_NATIVE_CASE "));
        assert.ok(summary, "native client did not produce verified ingestion evidence");
        results.push(JSON.parse(summary.slice("AGENTOS_NATIVE_CASE ".length)));
      }
    }
  } catch (error) {
    error.message += `\nNative engine output:\n${output}`;
    throw error;
  } finally {
    process.removeListener("SIGINT", cancel);
    process.removeListener("SIGTERM", cancel);
    if (child.exitCode === null && child.signalCode === null) child.kill("SIGTERM");
    try { await bounded("native engine exit", exited); } catch {
      if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
      await bounded("native engine forced exit", exited);
    }
    assert.equal(await listening(port), false, "native engine listener remained after teardown");
    await rm(scratch, { recursive: true, force: true });
  }
  console.log(`AGENTOS_NATIVE_OTEL ${JSON.stringify({ engine_version: version, engine_sha256: createHash("sha256").update(await readFile(engine)).digest("hex"), isolated_runtime: true, real_engine_ingestion: true, cases: results, engine_exited: true, listener_closed: true, scratch_removed: true })}`);
}

async function main() {
  if (process.argv[2] === "--portless-child") {
    const port = Number(process.env.PORT);
    assert.ok(Number.isInteger(port) && port > 0 && port <= 65535);
    await runNative(port);
    return;
  }
  let portless;
  try { portless = await executable("portless"); } catch { /* CI without portless uses a kernel-assigned test port */ }
  if (portless) {
    const result = await execFileAsync(portless, ["run", "--name", `agentos-otel-${randomUUID().slice(0, 8)}`, process.execPath, self, "--portless-child"], {
      env: { ...process.env, PORTLESS_LAN: "0", PORTLESS_SYNC_HOSTS: "0", AGENTOS_OTEL_NODE_BIN: process.execPath, AGENTOS_OTEL_BUN_BIN: await executable("bun"), III_BIN: await executable(process.env.III_BIN ?? "iii") }, timeout: 120_000, maxBuffer: 1024 * 1024,
    });
    const summary = result.stdout.split("\n").find((line) => line.startsWith("AGENTOS_NATIVE_OTEL "));
    assert.ok(summary, "portless child did not complete native acceptance");
    console.log(summary);
  } else {
    await runNative(await availablePort());
  }
}

await main();
