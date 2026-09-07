import assert from "node:assert/strict";
import { EventEmitter, once } from "node:events";
import { createRequire } from "node:module";
import { createServer } from "node:http";
import { bounded, loadOtel } from "./otel-support.mjs";

const require = createRequire(import.meta.url);
const { WebSocketServer } = require("ws");

async function verifyTelemetry(format) {
  const { sdk, otel, internal, api, core } = await loadOtel(format);
  const server = createServer();
  const sockets = new WebSocketServer({ server });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const events = new EventEmitter();
  const frames = new Map();
  const requests = [];
  sockets.on("connection", (socket, request) => {
    socket.on("message", (data) => {
      if (request.url === "/otel") {
        const buffer = Buffer.from(data);
        const prefix = buffer.subarray(0, 4).toString();
        const payload = JSON.parse(buffer.subarray(4).toString());
        frames.set(prefix, [...frames.get(prefix) ?? [], payload]);
        events.emit("frame");
        return;
      }
      const message = JSON.parse(data.toString());
      requests.push(message);
      if (message.type === "invokefunction" && message.invocation_id) {
        socket.send(JSON.stringify({
          type: "invocationresult",
          invocation_id: message.invocation_id,
          result: message.data,
        }));
      }
    });
  });
  let worker;
  try {
    worker = sdk.registerWorker(`ws://127.0.0.1:${server.address().port}`, {
      workerName: "agentos-otel-fixture",
      enableMetricsReporting: false,
      invocationTimeoutMs: 2_000,
      reconnectionConfig: { maxRetries: 0 },
      otel: {
        enabled: true,
        serviceName: "agentos-otel-fixture",
        serviceVersion: "fixture",
        metricsEnabled: true,
        metricsExportIntervalMs: 60_000,
        spansFlushIntervalMs: 60_000,
        logsFlushIntervalMs: 60_000,
        logsBatchSize: 16,
        fetchInstrumentationEnabled: false,
        reconnectionConfig: { maxRetries: 0 },
      },
    });
    const tracer = internal.getTracer();
    const meter = internal.getMeter();
    const logger = otel.getLogger();
    assert.ok(tracer && meter && logger, "all three providers must initialize");
    const baggage = api.propagation.createBaggage({ tenant: { value: "fixture" } });
    const parent = api.propagation.setBaggage(api.ROOT_CONTEXT, baggage);
    let spanContext;
    await api.context.with(parent, () => tracer.startActiveSpan("agentos-otel-roundtrip", async (span) => {
      try {
        spanContext = span.spanContext();
        const result = await worker.trigger({
          function_id: "fixture::echo",
          payload: { message: "local fixture" },
        });
        assert.deepEqual(result, { message: "local fixture" });
        logger.emit({ body: "agentos-otel-log", severityNumber: 9, attributes: { local: true } });
        meter.createCounter("agentos.otel.count").add(3);
      } finally {
        span.end();
      }
    }));
    const request = requests.find((message) => message.function_id === "fixture::echo");
    assert.equal(request.traceparent, `00-${spanContext.traceId}-${spanContext.spanId}-01`);
    assert.equal(request.baggage, "tenant=fixture");
    const carrier = {};
    const propagator = new core.W3CBaggagePropagator();
    propagator.inject(parent, carrier, api.defaultTextMapSetter);
    const roundtrip = propagator.extract(api.ROOT_CONTEXT, carrier, api.defaultTextMapGetter);
    assert.equal(api.propagation.getBaggage(roundtrip).getEntry("tenant").value, "fixture");
    const invalid = new core.W3CTraceContextPropagator().extract(
      api.ROOT_CONTEXT, { traceparent: "not-a-trace-context" }, api.defaultTextMapGetter,
    );
    assert.equal(api.trace.getSpanContext(invalid), undefined);
    await bounded("telemetry flush", otel.flushOtel());
    await waitForSignals(frames, events);
    const resources = (prefix, key) => frames.get(prefix).flatMap((payload) => payload[key]);
    for (const [prefix, key] of [["OTLP", "resourceSpans"], ["MTRC", "resourceMetrics"], ["LOGS", "resourceLogs"]]) {
      assert.ok(resources(prefix, key).every((entry) => entry.resource.attributes.some(
        (attribute) => attribute.key === "service.name" && attribute.value.stringValue === "agentos-otel-fixture",
      )), `${prefix} resource identity must survive serialization`);
    }
    const spans = resources("OTLP", "resourceSpans").flatMap((entry) => entry.scopeSpans).flatMap((scope) => scope.spans);
    const exported = spans.find((span) => span.name === "agentos-otel-roundtrip");
    assert.equal(exported.traceId, spanContext.traceId);
    assert.equal(exported.spanId, spanContext.spanId);
    const logs = resources("LOGS", "resourceLogs").flatMap((entry) => entry.scopeLogs).flatMap((scope) => scope.logRecords);
    const log = logs.find((entry) => entry.body.stringValue === "agentos-otel-log");
    assert.equal(log.traceId, spanContext.traceId);
    assert.equal(log.spanId, spanContext.spanId);
    const metrics = resources("MTRC", "resourceMetrics").flatMap((entry) => entry.scopeMetrics).flatMap((scope) => scope.metrics);
    const counter = metrics.find((metric) => metric.name === "agentos.otel.count");
    assert.equal(counter.sum.dataPoints[0].asDouble ?? counter.sum.dataPoints[0].asInt, 3);
  } finally {
    await bounded("SDK shutdown", worker?.shutdown());
    await bounded("telemetry shutdown", otel.shutdownOtel());
    for (const client of sockets.clients) client.terminate();
    sockets.close();
    server.closeAllConnections();
    if (server.listening) {
      await bounded("fixture server close", new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve(undefined))));
    }
    assert.equal(server.listening, false);
  }
  console.log(`AGENTOS_OTEL_OK ${format}`);
}

await verifyTelemetry(process.argv[2]);
function waitForSignals(frames, events) {
  return new Promise((resolve, reject) => {
    const cleanup = () => { clearTimeout(timeout); events.off("frame", check); };
    const check = () => {
      if (["OTLP", "MTRC", "LOGS"].every((prefix) => frames.has(prefix))) {
        cleanup();
        resolve(undefined);
      }
    };
    const timeout = setTimeout(() => {
      cleanup();
      reject(new Error(`Incomplete telemetry; received ${[...frames.keys()].join(", ")}`));
    }, 5_000);
    events.on("frame", check);
    check();
  });
}
