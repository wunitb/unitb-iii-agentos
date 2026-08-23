import { describe, expect, it } from "bun:test";

const repository = new URL("../", import.meta.url);
const legacyFunctions = ["complete", "route", "providers", "usage"].map(
  (name) => `llm::${name}`,
);

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

    for (const name of ["complete", "route", "providers", "usage"]) {
      expect(router).toContain(`"agentos::llm::${name}"`);
      expect(router).not.toContain(`"llm::${name}"`);
    }
  });

  it("has no source call sites for the four colliding legacy ids", async () => {
    const glob = new Bun.Glob("**/*.{rs,ts,tsx,js,jsx}");
    for await (const path of glob.scan({ cwd: repository.pathname })) {
      if (
        path === "tests/llm_namespace.test.ts" ||
        path.startsWith("target/") ||
        path.startsWith("node_modules/")
      ) {
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
      if (
        path === "tests/llm_namespace.test.ts" ||
        path.startsWith("target/") ||
        path.startsWith("node_modules/")
      ) {
        continue;
      }
      const calls = legacyLlmCalls(
        await Bun.file(new URL(path, repository)).text(),
      );
      if (calls.length > 0) failures.push(`${path}: ${calls.join(", ")}`);
    }

    expect(failures).toEqual([]);
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
