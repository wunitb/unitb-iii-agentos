// An exit-0 Vitest run may contain only skipped tests. Require the actual chat
// leaf, not the similarly named configuration preflight, to have passed.
import { readFileSync } from "node:fs";

const requiredTitle = "agent::chat — local fake Anthropic provider";
const requiredFullName = `AgentOS full-stack E2E ${requiredTitle}`;

function fail(message: string): never {
  console.error(`e2e result gate: ${message}`);
  process.exit(1);
}

function object(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined;
}

const reportFile = process.argv[2];
if (!reportFile || process.argv.length !== 3) {
  fail("usage: bun scripts/assert-e2e-results.ts <vitest-report.json>");
}

let parsed: unknown;
try {
  parsed = JSON.parse(readFileSync(reportFile, "utf8"));
} catch {
  // Parser errors can quote input, including captured provider credentials.
  fail("report is unreadable or invalid JSON");
}

const report = object(parsed);
if (!report || report.success !== true || report.numFailedTests !== 0
    || report.numFailedTestSuites !== 0 || !Array.isArray(report.testResults)) {
  fail("report does not describe a successful test run");
}

const matches: Record<string, unknown>[] = [];
for (const entry of report.testResults) {
  const suite = object(entry);
  if (!suite || typeof suite.name !== "string" || !Array.isArray(suite.assertionResults)) {
    fail("report has malformed suite results");
  }
  const file = suite.name.replace(/\\/g, "/");
  if (file !== "e2e/full-stack.test.ts" && !file.endsWith("/e2e/full-stack.test.ts")) continue;
  for (const value of suite.assertionResults) {
    const assertion = object(value);
    if (assertion?.title === requiredTitle) matches.push(assertion);
  }
}

if (matches.length !== 1 || matches[0].status !== "passed"
    || matches[0].fullName !== requiredFullName) {
  fail("expected exactly one passed actual fake-provider chat test in e2e/full-stack.test.ts");
}
console.log("e2e result gate: actual fake-provider chat test passed");
