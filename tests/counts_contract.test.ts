import { describe, expect, it } from "bun:test";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import {
  collectCounts,
  collectUnresolvedRegistrations,
  findDrift,
  publishedNumbers,
  rustRegistrationIds,
  rustStringConstants,
  workerTableSites,
} from "../scripts/counts";

/**
 * On 2026-09-02 the repository advertised 267 functions (really 293), 1,413 /
 * 1,393 / 1,281 tests (really 1,500), 25 LLM providers (really 11), eight CI jobs
 * (really eleven) and seven engine workers (really eighteen), and no document
 * mentioned the `context-monitor` worker at all. Numbers drift because nothing
 * recomputes them. `scripts/counts.ts` recomputes all of them from the tree; this
 * suite makes the recomputation a gate instead of a chore.
 */

const counts = collectCounts();

describe("published counts", () => {
  it("matches the tree everywhere it is published", () => {
    const drift = findDrift(counts).map((item) =>
      `${item.file}${item.line > 0 ? `:${item.line}` : ""} ${item.label}: published ${item.found}, tree says ${item.expected}`,
    );
    expect(drift, "run `bun run counts:write`, or fix the worker tables by hand").toEqual([]);
  });

  it("points every published number at a file that exists and a pattern that matches", async () => {
    const sites = publishedNumbers(counts);
    expect(sites.length, "the published-number table has been gutted").toBeGreaterThan(25);

    const broken: string[] = [];
    for (const site of sites) {
      const file = Bun.file(new URL(`../${site.file}`, import.meta.url));
      if (!(await file.exists())) {
        broken.push(`${site.file} does not exist (${site.label})`);
        continue;
      }
      const matches = [...(await file.text()).matchAll(site.pattern)];
      if (matches.length !== site.occurrences) {
        broken.push(`${site.file}: ${site.label} matched ${matches.length} time(s), expected ${site.occurrences}`);
      }
    }
    expect(broken, "a published-number pattern no longer matches its document").toEqual([]);
  });

  it("derives the numbers from source, not from a constant table", () => {
    expect(counts.workerCount).toBe(counts.rustWorkerCount + counts.pythonWorkerCount);
    expect(counts.pythonWorkerCount).toBe(1);
    expect(counts.functionIdCount).toBeLessThanOrEqual(counts.functionRegistrationCount);
    expect(counts.functionRegistrationCount - counts.functionIdCount).toBe(
      [...counts.duplicateFunctionIds.values()].reduce((total, sites) => total + sites.length - 1, 0),
    );
    expect(counts.rustTestAttributes).toBeGreaterThan(counts.ignoredRustTests);
    expect(counts.providers).toBeGreaterThan(0);
    expect(counts.cliSubcommands).toContain("Doctor");
    expect(counts.cliSubcommands).toContain("Up");
    expect(counts.tuiScreens).toContain("Chat");
    expect(counts.ciJobs).toContain("rust");
    expect(counts.ciJobs).toContain("node-unit");
    expect(counts.engineWorkers).toContain("configuration");
    expect(counts.releaseTargets.length).toBeGreaterThan(0);
    expect(counts.repositorySlug).toBe("wunitb/unitb-iii-agentos");
  });

  it("lists every worker exactly once in every worker table", () => {
    const expected = counts.workers.map((worker) => worker.name).sort();
    for (const site of workerTableSites()) {
      const published = site.parse();
      expect([...published].sort(), `${site.file} worker table`).toEqual(expected);
      expect(new Set(published).size, `${site.file} lists a worker twice`).toBe(published.length);
    }
  });

  it("names the worker every document forgot", () => {
    // Regression for the concrete 2026-09-02 finding.
    expect(counts.workers.map((worker) => worker.name)).toContain("context-monitor");
    for (const site of workerTableSites()) {
      expect(site.parse(), `${site.file} still omits context-monitor`).toContain("context-monitor");
    }
  });
});

describe("registration extractor", () => {
  const ids = new Set(counts.functionRegistrations.map((site) => site.id));
  const root = fileURLToPath(new URL("../", import.meta.url));
  const read = (file: string): string => {
    try {
      return readFileSync(`${root}${file}`, "utf8");
    } catch {
      return "";
    }
  };

  it("resolves an id declared as a Rust const or static", () => {
    // The capability, proven without depending on which branch is checked out.
    const source = [
      'const STREAM_JOIN_FUNCTION: &str = "stream::authorize_join";',
      'static TRIM_MICRO_FUNCTION_ID: &\'static str = "context::trim_micro";',
      "fn main() {",
      "    iii.register_function(STREAM_JOIN_FUNCTION, handler);",
      "    iii.register_function(&TRIM_MICRO_FUNCTION_ID, handler);",
      '    iii.register_function("agent::chat", handler);',
      "    iii.register_function(SOME_UNKNOWN_CONST, handler);",
      "}",
    ].join("\n");

    const constants = rustStringConstants(source);
    expect(constants.get("STREAM_JOIN_FUNCTION")).toBe("stream::authorize_join");
    expect(constants.get("TRIM_MICRO_FUNCTION_ID")).toBe("context::trim_micro");

    expect(rustRegistrationIds(source, constants).map((found) => found.id)).toEqual([
      "stream::authorize_join",
      "context::trim_micro",
      "agent::chat",
    ]);
  });

  it("counts a const-declared id wherever the tree actually has one", () => {
    // Bidirectional: the extractor must find what the source declares, and must
    // not invent what it does not. Green on a branch with the worker and without.
    for (const [file, constant, id] of [
      ["workers/streaming/src/main.rs", "STREAM_JOIN_FUNCTION", "stream::authorize_join"],
      ["workers/context-monitor/src/main.rs", "TRIM_MICRO_FUNCTION_ID", "context::trim_micro"],
    ] as const) {
      const source = read(file);
      const registersConstant =
        source.includes(`${constant}: &str`) && new RegExp(`register_function\\(\\s*&?${constant}`).test(source);
      expect(ids.has(id), `${file} ${registersConstant ? "registers" : "does not register"} ${id}`).toBe(
        registersConstant,
      );
    }
  });

  it("counts the Python worker's registrations", () => {
    expect(ids, "workers/embedding/main.py registers embedding::generate").toContain("embedding::generate");
    expect(ids, "workers/embedding/main.py registers embedding::similarity").toContain("embedding::similarity");
    expect(
      counts.functionRegistrations.filter((site) => site.file.endsWith(".py")).map((site) => site.file),
    ).toEqual(["workers/embedding/main.py", "workers/embedding/main.py"]);
  });

  it("does not scan the Python worker's test file", () => {
    expect(counts.functionRegistrations.some((site) => site.file.includes("test_"))).toBe(false);
  });

  it("leaves exactly the two runtime-built ids unresolved", () => {
    // A new unresolved shape must fail here rather than silently shrink the count.
    expect(collectUnresolvedRegistrations().map((site) => `${site.file}: ${site.argument}`).sort()).toEqual([
      "crates/http-adapter/src/lib.rs: adapter_id.clone()",
      'workers/hand-runner/src/main.rs: format!("hand::run::{hand_id}")',
    ]);
  });
});
