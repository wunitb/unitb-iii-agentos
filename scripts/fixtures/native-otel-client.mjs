import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { setTimeout as delay } from "node:timers/promises";
import { bounded, loadOtel } from "./otel-support.mjs";

async function queryUntil(client, functionId, payload, accept) {
  let result;
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    result = await client.trigger({ function_id: functionId, payload, timeoutMs: 2_000 });
    if (accept(result)) return result;
    await delay(50);
  }
  throw new Error(`${functionId} did not store the expected telemetry: ${JSON.stringify(result)}`);
}

async function main() {
  const format = process.argv[2];
  const { sdk, otel, internal, api } = await loadOtel(format);
  const marker = `agentos-native-otel-${randomUUID()}`;
  const metricName = "agentos.native.otel.count";
  const client = sdk.registerWorker(process.env.III_URL, {
    workerName: marker,
    enableMetricsReporting: false,
    invocationTimeoutMs: 2_000,
    reconnectionConfig: { maxRetries: 0 },
    otel: {
      enabled: true, serviceName: marker, serviceVersion: "acceptance",
      metricsEnabled: true, metricsExportIntervalMs: 60_000,
      spansFlushIntervalMs: 60_000, logsFlushIntervalMs: 60_000,
      logsBatchSize: 16, fetchInstrumentationEnabled: false,
      reconnectionConfig: { maxRetries: 0 },
    },
  });
  try {
    const echo = `${marker}::echo`;
    client.registerFunction(echo, async (payload) => ({ marker: payload.marker }));
    await queryUntil(client, "engine::functions::list", {}, (result) =>
      result.functions?.some((entry) => entry.function_id === echo));
    let spanContext;
    const parent = api.propagation.setBaggage(api.ROOT_CONTEXT,
      api.propagation.createBaggage({ "iii.tag.acceptance": { value: marker } }));
    await api.context.with(parent, () => internal.getTracer().startActiveSpan(marker, async (span) => {
      try {
        spanContext = span.spanContext();
        assert.deepEqual(await client.trigger({ function_id: echo, payload: { marker } }), { marker });
        otel.getLogger().emit({ body: marker, severityNumber: 9, severityText: "INFO", attributes: { acceptance: true } });
        internal.getMeter().createCounter(metricName).add(3, { acceptance: marker });
      } finally {
        span.end();
      }
    }));
    await bounded("native telemetry flush", otel.flushOtel());
    // iii 0.23 lists compact trace summaries; full attributes and IDs live in spans.
    const trace = await queryUntil(client, "engine::traces::spans",
      { trace_id: spanContext.traceId, include_internal: true, limit: 100 },
      (result) => result.spans?.some((span) => span.name === marker));
    const storedSpan = trace.spans.find((span) => span.name === marker);
    assert.equal(storedSpan.trace_id, spanContext.traceId);
    assert.equal(storedSpan.span_id, spanContext.spanId);
    assert.equal(storedSpan.service_name, marker);
    assert.ok(storedSpan.attributes.some(([key, value]) => key === "iii.tag.acceptance" && value === marker));
    const logs = await queryUntil(client, "engine::logs::list",
      { trace_id: spanContext.traceId, span_id: spanContext.spanId, limit: 100 },
      (result) => result.logs?.some((log) => log.body === marker));
    const log = logs.logs.find((entry) => entry.body === marker);
    assert.equal(log.trace_id, spanContext.traceId);
    assert.equal(log.span_id, spanContext.spanId);
    assert.equal(log.service_name, marker);
    assert.equal(log.resource["service.name"], marker);
    const metrics = await queryUntil(client, "engine::metrics::list", { metric_name: metricName },
      (result) => result.sdk_metrics?.some((metric) => metric.name === metricName && metric.service_name === marker));
    const metric = metrics.sdk_metrics.find((entry) => entry.name === metricName && entry.service_name === marker);
    assert.ok(metric.data_points.some((point) => point.value === 3), JSON.stringify(metric));
    console.log(`AGENTOS_NATIVE_CASE ${JSON.stringify({ runtime: process.versions.bun ? "bun" : "node", version: process.versions.bun ?? process.versions.node, format, traces: true, correlated_logs: true, metrics: true, rpc: true })}`);
  } finally {
    await bounded("native SDK shutdown", client.shutdown());
  }
}

await main();
