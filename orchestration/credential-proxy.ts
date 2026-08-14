import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import type { AuthIdentity, CredentialProxyConfig } from "./fleet-core";

export interface CredentialAssignment extends AuthIdentity {
  provider: string;
  credentialSlot: number;
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
}

interface SnapshotResponse {
  credentials: SnapshotEntry[];
  [key: string]: unknown;
}

const EMPTY_GET_ROUTES: Record<string, string> = {
  "/v1/usage": "reports",
  "/v1/usage/history": "entries",
  "/v1/usage/clients": "clients",
  "/v1/credentials/disabled": "disabled",
};

function json(body: unknown, status = 200, headers?: HeadersInit): Response {
  return Response.json(body, { status, headers });
}

function bearerToken(request: Request): string {
  const authorization = request.headers.get("authorization") ?? "";
  const match = /^Bearer\s+(.+)$/i.exec(authorization);
  if (!match?.[1]) throw new Error("Missing fleet bearer token");
  return match[1];
}

function credentialId(pathname: string): number | undefined {
  const match = /^\/v1\/credential\/(\d+)\/(?:refresh|disable|block|blocks)$/.exec(pathname);
  return match ? Number(match[1]) : undefined;
}

function assignedEntry(snapshot: SnapshotResponse, assignment: CredentialAssignment): SnapshotEntry | undefined {
  return snapshot.credentials
    .filter((candidate) => candidate.provider === assignment.provider)
    .sort((left, right) => left.id - right.id)[assignment.credentialSlot];
}

function noAssignedSlot(assignment: CredentialAssignment): Response {
  return json({ error: `No active credential slot ${assignment.credentialSlot} for ${assignment.provider}` }, 503);
}

function errorResponse(error: unknown): Response {
  const message = error instanceof Error ? error.message : String(error);
  const status = message === "Invalid fleet token" || message === "Missing fleet bearer token" ? 401 : 503;
  return json({ error: message }, status);
}

export function createCredentialProxyHandler(options: CredentialProxyOptions): (request: Request) => Promise<Response> {
  const upstreamUrl = options.upstreamUrl.replace(/\/$/, "");

  const upstream = async (request: Request, path = new URL(request.url).pathname + new URL(request.url).search): Promise<Response> => {
    const headers = new Headers(request.headers);
    headers.set("authorization", `Bearer ${options.upstreamToken}`);
    headers.delete("host");
    headers.delete("content-length");
    return fetch(`${upstreamUrl}${path}`, {
      method: request.method,
      headers,
      body: request.method === "GET" || request.method === "HEAD" ? undefined : await request.arrayBuffer(),
      signal: request.signal,
    });
  };

  const snapshotFor = async (assignment: CredentialAssignment): Promise<{ response: Response; entry?: SnapshotEntry }> => {
    const response = await fetch(`${upstreamUrl}/v1/snapshot`, {
      headers: { authorization: `Bearer ${options.upstreamToken}` },
    });
    if (!response.ok) return { response };
    return { response, entry: assignedEntry(await response.json() as SnapshotResponse, assignment) };
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
        if (response.status === 304 || !response.ok) return response;
        const snapshot = await response.json() as SnapshotResponse;
        const entry = assignedEntry(snapshot, assignment);
        if (!entry) return noAssignedSlot(assignment);
        const etag = response.headers.get("etag");
        return json({ ...snapshot, credentials: [entry] }, 200, etag ? { etag } : undefined);
      }

      if (request.method === "GET") {
        const emptyCollection = EMPTY_GET_ROUTES[url.pathname];
        if (emptyCollection) return json({ generatedAt: Date.now(), [emptyCollection]: [] });
      }
      if (request.method === "POST" && url.pathname === "/v1/credential") {
        return json({ error: "Worker credential uploads are forbidden" }, 403);
      }
      if (request.method === "POST" && (url.pathname === "/v1/usage/stale" || url.pathname === "/v1/usage/observed")) {
        return upstream(request);
      }

      const requestedCredentialId = credentialId(url.pathname);
      if (requestedCredentialId !== undefined) {
        const { response, entry } = await snapshotFor(assignment);
        if (!response.ok) return response;
        if (!entry) return noAssignedSlot(assignment);
        if (entry.id !== requestedCredentialId) return json({ error: "Credential is not assigned to this fleet identity" }, 403);
        return upstream(request);
      }

      return json({ error: "Unsupported auth-broker route" }, 404);
    } catch (error) {
      return errorResponse(error);
    }
  };
}

export function startCredentialProxy(
  config: CredentialProxyConfig,
  resolveAssignment: (token: string) => CredentialAssignment,
): CredentialProxyServer {
  const tokenPath = config.upstreamTokenFile.startsWith("~/")
    ? resolve(process.env.HOME ?? "", config.upstreamTokenFile.slice(2))
    : resolve(config.upstreamTokenFile);
  const upstreamToken = readFileSync(tokenPath, "utf8").trim();
  if (!upstreamToken) throw new Error(`Empty upstream auth-broker token: ${tokenPath}`);
  const separator = config.bind.lastIndexOf(":");
  if (separator < 1) throw new Error(`Invalid credentialProxy.bind: ${config.bind}`);
  const hostname = config.bind.slice(0, separator);
  const port = Number(config.bind.slice(separator + 1));
  if (!Number.isSafeInteger(port) || port < 1 || port > 65535) {
    throw new Error(`Invalid credentialProxy.bind port: ${config.bind}`);
  }
  return Bun.serve({
    hostname,
    port,
    fetch: createCredentialProxyHandler({ upstreamUrl: config.upstreamUrl, upstreamToken, resolveAssignment }),
  });
}
