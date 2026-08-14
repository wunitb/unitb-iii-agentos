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
