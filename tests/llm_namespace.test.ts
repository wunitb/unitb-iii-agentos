import { describe, expect, it } from "bun:test";

const repository = new URL("../", import.meta.url);
const legacyFunctions = ["complete", "route", "providers", "usage"].map(
  (name) => `llm::${name}`,
);

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

  it("has no call sites for the four colliding legacy ids", async () => {
    const glob = new Bun.Glob("workers/**/src/*.rs");
    for await (const path of glob.scan({ cwd: repository.pathname })) {
      const source = await Bun.file(new URL(path, repository)).text();
      for (const legacy of legacyFunctions) {
        expect(source).not.toContain(`"${legacy}"`);
      }
    }
  });
});
