import { describe, expect, it } from "bun:test";
import { collectCounts, findDrift, publishedNumbers, workerTableSites } from "../scripts/counts";

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
