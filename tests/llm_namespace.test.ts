import { describe, expect, it } from "bun:test";

const repository = new URL("../", import.meta.url);
const agentosLlmFunctions = ["complete", "route", "providers", "usage"];
const legacyFunctions = ["chat", ...agentosLlmFunctions].map(
  (name) => `llm::${name}`,
);

const excludedSourcePrefixes = [
  "target/",
  "node_modules/",
  "website/node_modules/",
  "website/dist/",
  "dist/",
  "coverage/",
  ".upstream-iii/",
] as const;

function isRepositorySource(path: string): boolean {
  return !excludedSourcePrefixes.some((prefix) => path.startsWith(prefix));
}

function legacyLlmCalls(source: string): string[] {
  const namespace = ["llm", "::"].join("");
  const quotedFunctionId = new RegExp(
    `["'\\x60](${namespace}[A-Za-z0-9_-]+)["'\\x60]`,
    "g",
  );
  return [...source.matchAll(quotedFunctionId)].map((match) => match[1]);
}

describe("AgentOS LLM namespace", () => {
  it("registers its router functions below agentos::llm", async () => {
    const router = await Bun.file(
      new URL("workers/llm-router/src/main.rs", repository),
    ).text();

    for (const name of agentosLlmFunctions) {
      expect(router).toContain(`"agentos::llm::${name}"`);
      expect(router).not.toContain(`"llm::${name}"`);
    }
  });

  it("has no source call sites for colliding legacy llm ids", async () => {
    const glob = new Bun.Glob("**/*.{rs,ts,tsx,js,jsx}");
    for await (const path of glob.scan({ cwd: repository.pathname })) {
      if (path === "tests/llm_namespace.test.ts" || !isRepositorySource(path)) {
        continue;
        }
      const source = await Bun.file(new URL(path, repository)).text();
      for (const legacy of legacyFunctions) {
        expect(source, `${path} still references ${legacy}`).not.toMatch(
          new RegExp(`["'\\x60]${legacy}["'\\x60]`),
        );
      }
    }
  });

  it("has no consumers in any legacy llm namespace", async () => {
    const failures: string[] = [];
    const glob = new Bun.Glob("**/*.{rs,ts,tsx,js,jsx}");
    for await (const path of glob.scan({ cwd: repository.pathname })) {
      if (path === "tests/llm_namespace.test.ts" || !isRepositorySource(path)) {
        continue;
        }
      const calls = legacyLlmCalls(
        await Bun.file(new URL(path, repository)).text(),
      );
      if (calls.length > 0) failures.push(`${path}: ${calls.join(", ")}`);
    }

    expect(failures).toEqual([]);
  });

  it("routes canonical consumers through complete without a fake default model", async () => {
    for (const path of [
      "workers/orchestrator/src/main.rs",
      "workers/task-decomposer/src/main.rs",
    ]) {
      const source = await Bun.file(new URL(path, repository)).text();
      expect(source).toContain('function_id: "agentos::llm::complete"');
      expect(source).not.toContain("agentos::llm::chat");
      expect(source).not.toContain('unwrap_or("default")');
    }
  });

  it("legacy namespace detection handles empty, near-miss, and boundary input", () => {
    expect(legacyLlmCalls("")).toEqual([]);
    expect(legacyLlmCalls("llm::chat and agentos::llm::chat")).toEqual([]);
    expect(legacyLlmCalls("call(\"llm::chat\")\n'llm::route'\n`llm::complete`")).toEqual([
      "llm::chat",
      "llm::route",
      "llm::complete",
    ]);
  });

  it("records the complementary collision-error proposal as upstream-only", async () => {
    const decision = await Bun.file(
      new URL("docs/decisions/2026-08-22-salvage-batch.md", repository),
    ).text();

    expect(decision).toContain("NOTE for the principal");
    expect(decision).toContain("belongs upstream in `iii-hq/iii`");
    expect(decision).toContain("Decide whether to open that upstream issue");
    expect(decision).toContain("does not post externally");
  });
});
