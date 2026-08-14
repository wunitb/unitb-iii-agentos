#!/usr/bin/env bun
import { chmodSync, existsSync, rmSync } from "node:fs";
import { createConnection, createServer } from "node:net";

const ALLOWED_METHODS: Record<string, true> = {
  "pane.report_agent": true,
  "pane.report_agent_session": true,
  "pane.release_agent": true,
};

function argument(name: string): string {
  const index = Bun.argv.indexOf(name);
  const value = index >= 0 ? Bun.argv[index + 1] : undefined;
  if (!value) throw new Error(`${name} is required`);
  return value;
}

function validateRequest(raw: string, paneId: string): string {
  const request = JSON.parse(raw) as { id?: unknown; method?: unknown; params?: Record<string, unknown> };
  if (typeof request.id !== "string" || request.id.length === 0) throw new Error("Request ID is required");
  if (typeof request.method !== "string" || !ALLOWED_METHODS[request.method]) throw new Error("Herdr method is not allowed");
  if (!request.params || request.params.pane_id !== paneId) throw new Error("Herdr pane binding mismatch");
  if (request.params.source !== "herdr:omp" || request.params.agent !== "omp") {
    throw new Error("Herdr reporter identity mismatch");
  }
  if ("state" in request.params && !["working", "blocked", "idle"].includes(String(request.params.state))) {
    throw new Error("Invalid Herdr lifecycle state");
  }
  return `${JSON.stringify(request)}\n`;
}

function forward(upstreamPath: string, payload: string): Promise<string> {
  const { promise, resolve, reject } = Promise.withResolvers<string>();
  const upstream = createConnection(upstreamPath);
  let response = "";
  const timeout = setTimeout(() => {
    upstream.destroy();
    reject(new Error("Upstream Herdr timeout"));
  }, 2_000);
  upstream.setEncoding("utf8");
  upstream.on("connect", () => upstream.write(payload));
  upstream.on("data", (chunk) => {
    response += chunk;
    if (response.includes("\n")) {
      clearTimeout(timeout);
      upstream.end();
      resolve(response.slice(0, response.indexOf("\n") + 1));
    }
  });
  upstream.on("error", (error) => {
    clearTimeout(timeout);
    reject(error);
  });
  return promise;
}

const listenPath = argument("--listen");
const upstreamPath = argument("--upstream");
const paneId = argument("--pane");
if (existsSync(listenPath)) rmSync(listenPath);

const server = createServer((client) => {
  client.setEncoding("utf8");
  let buffer = "";
  client.on("data", (chunk) => {
    buffer += chunk;
    const newline = buffer.indexOf("\n");
    if (newline < 0) {
      if (buffer.length > 128_000) client.destroy(new Error("Request too large"));
      return;
    }
    const raw = buffer.slice(0, newline);
    buffer = "";
    void (async () => {
      try {
        const payload = validateRequest(raw, paneId);
        const response = await forward(upstreamPath, payload);
        client.end(response);
      } catch (error) {
        console.error(error instanceof Error ? error.message : String(error));
        client.end(`${JSON.stringify({ error: error instanceof Error ? error.message : String(error) })}\n`);
      }
    })();
  });
});

server.listen(listenPath, () => chmodSync(listenPath, 0o600));
const shutdown = () => {
  server.close();
  if (existsSync(listenPath)) rmSync(listenPath);
  process.exit(0);
};
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
