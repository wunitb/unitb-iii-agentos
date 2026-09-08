import { readFileSync } from "node:fs";

const required = ["registry", "worker-identities", "access-control", "fake-chat", "fake-protocol", "realm", "mission", "security", "wasm"];

function fail(message: string): never {
  console.error(`OCI result gate: ${message}`);
  process.exit(1);
}

const file = process.argv[2];
if (!file || process.argv.length !== 3) fail("usage: bun scripts/assert-oci-results.ts <report.json>");
let value: unknown;
try {
  value = JSON.parse(readFileSync(file, "utf8"));
} catch {
  fail("report is unreadable or invalid JSON");
}
if (!value || typeof value !== "object" || Array.isArray(value)) fail("invalid report shape");
const report = value as Record<string, unknown>;
if (report.schema !== "agentos-oci-acceptance/v1" || report.mode !== "fixture" || report.success !== true) {
  fail("report is not a successful fixture acceptance run");
}
if (typeof report.engine !== "string" || !/^\d+\.\d+\.\d+$/.test(report.engine)
    || report.engine !== readFileSync(new URL("../.iii-version", import.meta.url), "utf8").trim()
    || typeof report.image !== "string" || !/^(sha256:)?[0-9a-f]{64}$/.test(report.image)) {
  fail("report lacks stable engine and immutable image identity");
}
if (!Array.isArray(report.checks) || report.checks.length !== required.length
    || new Set(report.checks).size !== required.length
    || required.some((name) => !(report.checks as unknown[]).includes(name))) {
  fail("report lacks the complete actual acceptance checks");
}
if (report.provider_requests !== 1 || report.restart !== true || report.data_preserved !== true
    || report.key_preserved !== true || report.teardown !== true) {
  fail("provider execution, restart, persistence or teardown was not verified");
}
console.log("OCI result gate: actual fixture chat, persistence and teardown passed");
