import type { IIIClient, JsonValue } from "iii-sdk";

export const ENGINE_URL = process.env.III_URL ?? "ws://localhost:49134";

export const OTEL_CONFIG = {
  enabled: process.env.OTEL_ENABLED !== "false",
};

type HttpTriggerConfig = {
  api_path: string;
  http_method: string;
};

type JsonObject = Record<string, JsonValue>;

function isJsonObject(value: JsonValue | undefined): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function normalizeHttpConfig(config: HttpTriggerConfig): HttpTriggerConfig {
  const apiPath = config.api_path.startsWith("/")
    ? config.api_path
    : `/${config.api_path}`;

  return {
    api_path: apiPath,
    http_method: config.http_method.toUpperCase(),
  };
}

function normalizeHttpRequest(request: JsonValue): JsonValue {
  if (!isJsonObject(request)) {
    return request;
  }

  const payload: JsonObject = { ...request };

  if (isJsonObject(request.body)) {
    for (const [key, value] of Object.entries(request.body)) {
      payload[key] ??= value;
    }
  }

  if (isJsonObject(request.query_params)) {
    for (const [key, value] of Object.entries(request.query_params)) {
      payload[key] ??=
        Array.isArray(value) && value.length === 1 ? value[0] : value;
    }
    payload.query = request.query_params;
  }

  if (isJsonObject(request.path_params)) {
    for (const [key, value] of Object.entries(request.path_params)) {
      payload[key] = value;
    }
  }

  return payload;
}

function isHttpResponse(value: JsonValue): boolean {
  return (
    isJsonObject(value) &&
    typeof value.status_code === "number" &&
    Object.hasOwn(value, "body")
  );
}

export function registerHttpTrigger(
  iii: IIIClient,
  functionId: string,
  config: HttpTriggerConfig,
): void {
  const normalizedConfig = normalizeHttpConfig(config);
  const adapterId = `agentos::http::${functionId}::${normalizedConfig.http_method}::${normalizedConfig.api_path}`;

  iii.registerFunction(
    adapterId,
    async (request: JsonValue) => {
      const result = (await iii.trigger({
        function_id: functionId,
        payload: normalizeHttpRequest(request),
      })) as JsonValue;

      return isHttpResponse(result)
        ? result
        : { status_code: 200, body: result };
    },
    { description: `HTTP adapter for ${functionId}` },
  );

  iii.registerTrigger({
    type: "http",
    function_id: adapterId,
    config: normalizedConfig,
  });
}

export function registerShutdown(iii: IIIClient): void {
  let shuttingDown = false;

  const shutdown = () => {
    if (shuttingDown) return;
    shuttingDown = true;
    void iii.shutdown().finally(() => process.exit(0));
  };

  process.once("SIGINT", shutdown);
  process.once("SIGTERM", shutdown);
}

type AgentStateWrite = {
  scope: string;
  key: string;
  value: JsonValue;
};

/**
 * The `state::set` writes that register one agent.
 *
 * Two properties of the engine's state protocol (iii 0.22.1) shape this:
 *
 * 1. `state::list` answers a bare array of the stored values with no keys, so
 *    the agent id must live inside the agent document. Readers such as
 *    `a2a::list_cards` and `lifecycle::check_all` look for `id`.
 * 2. Capabilities are read from their own scope (`capabilities`, key
 *    `<agentId>`, value `{ tools, updatedAt }`), not from the agent document,
 *    so an agent registered without that document can call no tool at all.
 */
export function agentStateWrites(
  agentId: string,
  config: Record<string, JsonValue>,
  now: number,
): AgentStateWrite[] {
  const capabilities = config.capabilities;
  const tools =
    isJsonObject(capabilities) && Array.isArray(capabilities.tools)
      ? capabilities.tools
      : [];

  return [
    { scope: "agents", key: agentId, value: { ...config, id: agentId } },
    {
      scope: "capabilities",
      key: agentId,
      value: { tools, updatedAt: now },
    },
  ];
}
