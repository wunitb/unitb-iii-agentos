import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { describe, expect, it } from "bun:test";

const execFileAsync = promisify(execFile);

// Only a kernel-assigned local WebSocket fixture is used. The separate OTEL
// compatibility matrix verifies all three signals; this guards auth and routing.
const fixture = String.raw`
import assert from "node:assert/strict";
import { once } from "node:events";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { setTimeout as delay } from "node:timers/promises";
const require = createRequire(import.meta.url);
const sdk = process.env.SDK_FORMAT === "cjs" ? require("iii-sdk") : await import("iii-sdk");
const { workerOptions } = await import("./examples/shared.ts");
const { WebSocketServer } = require("ws");
const { bounded } = await import("./scripts/fixtures/otel-support.mjs");
const server = createServer();
const sockets = new WebSocketServer({ server });
server.listen(0, "127.0.0.1");
await once(server, "listening");
const requests = [];
let authorization;
sockets.on("connection", (socket, request) => {
  authorization = request.headers.authorization;
  socket.on("message", (data) => {
    const message = JSON.parse(data.toString());
    requests.push(message);
    if (message.type === "invokefunction" && message.invocation_id) {
      socket.send(JSON.stringify({ type: "invocationresult", invocation_id: message.invocation_id, result: message.data }));
    }
  });
});
let client;
try {
  client = sdk.registerWorker("ws://127.0.0.1:" + server.address().port, {
    ...workerOptions("agentos-sdk-contract", { AGENTOS_API_KEY: "fixture-bus-key", UNRELATED_SECRET: "must-not-cross" }),
    enableMetricsReporting: false,
    invocationTimeoutMs: 2000,
    reconnectionConfig: { maxRetries: 0 },
    otel: { enabled: false },
  });
  client.registerFunction("agentos::fixture::echo", async (payload) => payload);
  client.registerTrigger({ type: "http", function_id: "agentos::fixture::echo", config: { api_path: "/fixture", http_method: "POST" } });
  assert.deepEqual(await client.trigger({ function_id: "agentos::fixture::echo", payload: { local: true } }), { local: true });
  await client.trigger({ function_id: "engine::functions::list", payload: {} });
  const deadline = Date.now() + 2000;
  while (!requests.some((entry) => entry.function_id === "engine::workers::register")) {
    assert.ok(Date.now() < deadline, "worker identity was not registered");
    await delay(10);
  }
  assert.equal(authorization, "Bearer fixture-bus-key");
  const metadata = requests.find((entry) => entry.function_id === "engine::workers::register");
  assert.equal(metadata.data.name, "agentos-sdk-contract");
  assert.equal(metadata.data.namespace, "agentos-sdk-fixture");
  const call = requests.find((entry) => entry.type === "invokefunction" && entry.function_id === "agentos::fixture::echo");
  assert.equal(call.namespace, "agentos-sdk-fixture");
  const builtin = requests.find((entry) => entry.function_id === "engine::functions::list");
  assert.equal(builtin.namespace, "default");
  assert.ok(requests.some((entry) => entry.type === "registerfunction" && entry.id === "agentos::fixture::echo"));
  const trigger = requests.find((entry) => entry.message_type === "registertrigger");
  assert.ok(trigger, "HTTP binding was not registered");
  assert.equal(trigger.namespace, "agentos-sdk-fixture");
  assert.equal(trigger.trigger_namespace, undefined, "keep engine provider namespace fallback");
  assert.ok(!JSON.stringify(requests).includes("must-not-cross"));
} finally {
  if (client) await bounded("SDK shutdown", client.shutdown());
  for (const socket of sockets.clients) socket.terminate();
  sockets.close();
  server.closeAllConnections();
  if (server.listening) await bounded("HTTP fixture shutdown", new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve())));
  assert.equal(server.listening, false);
}
console.log("AGENTOS_SDK_AUTH_NAMESPACE_OK");
`;

describe("released Node SDK auth and namespace contract", () => {
  it.each([
    ["node", "esm"], ["node", "cjs"], ["bun", "esm"], ["bun", "cjs"],
  ])("preserves identity, credential and namespace routing with %s/%s", async (runtime, format) => {
    const args = runtime === "bun"
      ? ["--no-env-file", "--eval", fixture]
      : ["--input-type=module", "--eval", fixture];
    const { stdout, stderr } = await execFileAsync(runtime, args, {
      cwd: new URL("../", import.meta.url),
      env: { PATH: process.env.PATH, SDK_FORMAT: format, III_NAMESPACE: "agentos-sdk-fixture" },
      timeout: 10_000,
    });
    expect(stderr).toBe("");
    expect(stdout).toContain("AGENTOS_SDK_AUTH_NAMESPACE_OK");
  }, 15_000);
});
