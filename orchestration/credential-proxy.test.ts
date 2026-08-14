import { afterEach, describe, expect, test } from "bun:test";
import { createCredentialProxyHandler } from "./credential-proxy";

interface StoppableServer {
  stop(closeActiveConnections?: boolean): void;
}

const servers: StoppableServer[] = [];

function fakeBroker() {
  const calls: Array<{ method: string; path: string; body: string }> = [];
  const server = Bun.serve({
    port: 0,
    async fetch(request) {
      const url = new URL(request.url);
      const body = request.method === "GET" ? "" : await request.text();
      calls.push({ method: request.method, path: url.pathname, body });
      if (request.headers.get("authorization") !== "Bearer upstream-secret") {
        return Response.json({ error: "unauthorized" }, { status: 401 });
      }
      if (url.pathname === "/v1/healthz") return Response.json({ ok: true });
      if (url.pathname === "/v1/snapshot") {
        return Response.json({
          generation: 7,
          serverNowMs: Date.now(),
          refresher: { enabled: true, intervalMs: 60_000, skewMs: 300_000, nextSweepInMs: 20_000 },
          credentials: [
            { id: 1, provider: "anthropic", credential: { type: "oauth", access: "first" }, rotatesInMs: 10_000 },
            { id: 2, provider: "anthropic", credential: { type: "oauth", access: "second" }, rotatesInMs: 10_000 },
            { id: 3, provider: "openai-codex", credential: { type: "oauth", access: "codex" }, rotatesInMs: 10_000 },
          ],
        }, { headers: { etag: "\"7\"" } });
      }
      if (url.pathname === "/v1/credential/2/refresh") {
        return Response.json({ entry: { id: 2, provider: "anthropic" } });
      }
      return Response.json({ ok: true });
    },
  });
  servers.push(server);
  return { calls, url: `http://127.0.0.1:${server.port}` };
}

function handler(upstreamUrl: string) {
  return createCredentialProxyHandler({
    upstreamUrl,
    upstreamToken: "upstream-secret",
    resolveAssignment(token) {
      if (token !== "team-secret") throw new Error("Invalid fleet token");
      return { id: "TEAM-01", role: "team", teamId: "TEAM-01", provider: "anthropic", credentialSlot: 1 };
    },
  });
}

afterEach(() => {
  for (const server of servers.splice(0)) server.stop(true);
});

describe("scoped credential proxy", () => {
  test("returns only the credential assigned to the authenticated identity", async () => {
    const broker = fakeBroker();
    const response = await handler(broker.url)(new Request("http://proxy/v1/snapshot", {
      headers: { authorization: "Bearer team-secret" },
    }));

    expect(response.status).toBe(200);
    expect(response.headers.get("etag")).toBe("\"7\"");
    const body = await response.json() as {
      credentials: Array<{
        id: number;
        provider: string;
        credential: { type: string; access: string };
        rotatesInMs: number;
      }>;
    };
    expect(body.credentials).toEqual([{ id: 2, provider: "anthropic", credential: { type: "oauth", access: "second" }, rotatesInMs: 10_000 }]);
  });

  test("rejects other identities and credentials while forwarding the assigned refresh", async () => {
    const broker = fakeBroker();
    const proxy = handler(broker.url);
    const unauthorized = await proxy(new Request("http://proxy/v1/snapshot", {
      headers: { authorization: "Bearer wrong" },
    }));
    const forbidden = await proxy(new Request("http://proxy/v1/credential/1/refresh", {
      method: "POST",
      headers: { authorization: "Bearer team-secret" },
    }));
    const allowed = await proxy(new Request("http://proxy/v1/credential/2/refresh", {
      method: "POST",
      headers: { authorization: "Bearer team-secret", "content-type": "application/json" },
      body: "{}",
    }));

    expect(unauthorized.status).toBe(401);
    expect(forbidden.status).toBe(403);
    expect(allowed.status).toBe(200);
    expect(broker.calls.at(-1)).toEqual({ method: "POST", path: "/v1/credential/2/refresh", body: "{}" });
  });

  test("does not expose cross-account usage or accept credential uploads", async () => {
    const broker = fakeBroker();
    const proxy = handler(broker.url);
    const usage = await proxy(new Request("http://proxy/v1/usage", {
      headers: { authorization: "Bearer team-secret" },
    }));
    const upload = await proxy(new Request("http://proxy/v1/credential", {
      method: "POST",
      headers: { authorization: "Bearer team-secret" },
      body: "secret",
    }));

    expect(await usage.json()).toEqual({ generatedAt: expect.any(Number), reports: [] });
    expect(upload.status).toBe(403);
    expect(broker.calls).toHaveLength(0);
  });
});
