import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

export async function loadOtel(format) {
  assert.ok(format === "esm" || format === "cjs");
  const load = (name) => format === "cjs" ? require(name) : import(name);
  const [sdk, otel, internal, api, core] = await Promise.all([
    load("iii-sdk"), load("@iii-dev/helpers/observability"),
    load("@iii-dev/helpers/observability/internal"), load("@opentelemetry/api"),
    load("@opentelemetry/core"),
  ]);
  return { sdk, otel, internal, api, core };
}

export async function bounded(label, promise, milliseconds = 5_000) {
  let timeout;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timeout = setTimeout(() => reject(new Error(`${label} timed out`)), milliseconds);
    })]);
  } finally {
    clearTimeout(timeout);
  }
}
