import { chmodSync, readFileSync, rmSync } from "node:fs";
import { resolve } from "node:path";
import type { AuthIdentity, CredentialProxyConfig } from "./fleet-core";

export interface CredentialAssignment extends AuthIdentity {
  provider: string;
  credentialId: number;
}

interface CredentialProxyOptions {
  upstreamUrl: string;
  upstreamToken: string;
  resolveAssignment(token: string): CredentialAssignment;
}

export interface CredentialProxyServer {
  stop(closeActiveConnections?: boolean): void;
}

interface SnapshotEntry {
  id: number;
  provider: string;
  [key: string]: unknown;
}

interface SnapshotResponse {
  credentials: SnapshotEntry[];
  [key: string]: unknown;
}
interface ObservedUsageRequest {
  entries?: Array<{ provider?: unknown }>;
}

const EMPTY_GET_ROUTES: Record<string, string> = {
  "/v1/usage": "reports",
  "/v1/usage/history": "entries",
  "/v1/usage/clients": "clients",
  "/v1/credentials/disabled": "disabled",
};

function json(body: unknown, status = 200, headers?: HeadersInit): Response {
  const responseHeaders = new Headers(headers);
  responseHeaders.set("content-type", "application/json");
  responseHeaders.delete("content-length");
  return new Response(JSON.stringify(body), { status, headers: responseHeaders });
}

function bearerToken(request: Request): string {
  const authorization = request.headers.get("authorization") ?? "";
  const match = /^Bearer\s+(.+)$/i.exec(authorization);
  if (!match?.[1]) throw new Error("Missing fleet bearer token");
  return match[1];
}

function credentialId(pathname: string): number | undefined {
  const match = /^\/v1\/credential\/(\d+)\/(?:refresh|block|blocks)$/.exec(pathname);
  return match ? Number(match[1]) : undefined;
}

function assignedEntry(entry: unknown, assignment: CredentialAssignment): entry is SnapshotEntry {
  if (!entry || typeof entry !== "object") return false;
  const candidate = entry as { id?: unknown; provider?: unknown };
  return candidate.id === assignment.credentialId && candidate.provider === assignment.provider;
}

function errorResponse(error: unknown): Response {
  const message = error instanceof Error ? error.message : String(error);
  const status = message === "Invalid fleet token" || message === "Missing fleet bearer token" ? 401 : 503;
  return json({ error: message }, status);
}

export function createCredentialProxyHandler(options: CredentialProxyOptions): (request: Request) => Promise<Response> {
  const upstreamUrl = options.upstreamUrl.replace(/\/$/, "");

  const upstream = async (request: Request): Promise<Response> => {
    const url = new URL(request.url);
    const headers = new Headers(request.headers);
    headers.set("authorization", `Bearer ${options.upstreamToken}`);
    headers.delete("host");
    headers.delete("content-length");
    return fetch(`${upstreamUrl}${url.pathname}${url.search}`, {
      method: request.method,
      headers,
      body: request.method === "GET" || request.method === "HEAD" ? undefined : await request.arrayBuffer(),
      signal: request.signal,
    });
  };

  return async (request: Request): Promise<Response> => {
    const url = new URL(request.url);
    try {
      if (url.pathname === "/v1/healthz") {
        const response = await upstream(request);
        return response.ok ? json({ ok: true, version: "unitb-fleet-scoped-v1" }) : response;
      }

      const assignment = options.resolveAssignment(bearerToken(request));
      if (url.pathname === "/v1/snapshot/stream") {
        return json({ error: "Snapshot streaming is disabled by the scoped proxy" }, 404);
      }
      if (request.method === "GET" && url.pathname === "/v1/snapshot") {
        const response = await upstream(request);
        if (!response.ok) return response;
        const snapshot = await response.json() as SnapshotResponse;
        if (!Array.isArray(snapshot.credentials)) return json({ error: "Malformed auth-broker snapshot" }, 502);
        const entry = snapshot.credentials.find((candidate) => assignedEntry(candidate, assignment));
        if (!entry) {
          return json({ error: `Credential id ${assignment.credentialId} is unavailable for ${assignment.provider}` }, 503);
        }
        return json({ ...snapshot, credentials: [entry] }, response.status, response.headers);
      }
      if (request.method === "GET") {
        const emptyCollection = EMPTY_GET_ROUTES[url.pathname];
        if (emptyCollection) return json({ generatedAt: Date.now(), [emptyCollection]: [] });
      }
      if (request.method === "POST" && url.pathname === "/v1/usage/observed") {
        const body = await request.clone().json() as ObservedUsageRequest;
        if (!Array.isArray(body.entries) || body.entries.some((entry) => entry.provider !== assignment.provider)) {
          return json({ error: "Observed usage is outside the assigned provider scope" }, 403);
        }
        return upstream(request);
      }

      const requestedCredentialId = credentialId(url.pathname);
      if (requestedCredentialId === undefined) {
        return json({ error: "Route is outside the assigned credential scope" }, 403);
      }
      if (requestedCredentialId !== assignment.credentialId) {
        return json({ error: "Credential is not assigned to this fleet identity" }, 403);
      }
      const response = await upstream(request);
      if (url.pathname.endsWith("/refresh") && response.ok) {
        const body = await response.json() as { entry?: unknown };
        if (!assignedEntry(body.entry, assignment)) {
          return json({ error: "Auth-broker returned an unassigned credential" }, 502);
        }
        return json(body, response.status, response.headers);
      }
      return response;
    } catch (error) {
      return errorResponse(error);
    }
  };
}

export function startCredentialProxy(
  config: CredentialProxyConfig,
  resolveAssignment: (token: string) => CredentialAssignment,
  unixPath?: string,
): CredentialProxyServer {
  const tokenPath = config.upstreamTokenFile.startsWith("~/")
    ? resolve(process.env.HOME ?? "", config.upstreamTokenFile.slice(2))
    : resolve(config.upstreamTokenFile);
  const upstreamToken = readFileSync(tokenPath, "utf8").trim();
  if (!upstreamToken) throw new Error(`Empty upstream auth-broker token: ${tokenPath}`);
  const handler = createCredentialProxyHandler({ upstreamUrl: config.upstreamUrl, upstreamToken, resolveAssignment });
  if (unixPath) {
    rmSync(unixPath, { force: true });
    const server = Bun.serve({ unix: unixPath, fetch: handler });
    chmodSync(unixPath, 0o600);
    return server;
  }
  const separator = config.bind.lastIndexOf(":");
  if (separator < 1) throw new Error(`Invalid credentialProxy.bind: ${config.bind}`);
  const hostname = config.bind.slice(0, separator);
  const port = Number(config.bind.slice(separator + 1));
  if (!Number.isSafeInteger(port) || port < 1 || port > 65535) {
    throw new Error(`Invalid credentialProxy.bind port: ${config.bind}`);
  }
  return Bun.serve({ hostname, port, fetch: handler });
}
