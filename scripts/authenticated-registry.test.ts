import { readFile } from "node:fs/promises";
import { describe, expect, it } from "vitest";

import { readGeneratedApiKey } from "./authenticated-registry";

const generatedKey = "0123456789abcdef".repeat(4);

describe("authenticated registry helper", () => {
  it("reads the one generated API key without accepting unrelated values", () => {
    expect(
      readGeneratedApiKey(
        `# generated runtime\nAUDIT_HMAC_KEY=${"f".repeat(64)}\nAGENTOS_API_KEY=${generatedKey}\n`,
      ),
    ).toBe(generatedKey);
  });

  it.each([
    ["missing", "AUDIT_HMAC_KEY=x\n"],
    ["empty", "AGENTOS_API_KEY=\n"],
    ["duplicate", `AGENTOS_API_KEY=${generatedKey}\nAGENTOS_API_KEY=${generatedKey}\n`],
    ["not generated hex", "AGENTOS_API_KEY=operator-value\n"],
  ])("rejects the %s generated-key contract", (_name, source) => {
    expect(() => readGeneratedApiKey(source)).toThrow(/generated AGENTOS_API_KEY/);
  });

  it("uses the pinned SDK with a bearer header and never reads a parent key", async () => {
    const source = await readFile(new URL("./authenticated-registry.ts", import.meta.url), "utf8");
    expect(source).toContain('await import("iii-sdk")');
    expect(source).toContain("Authorization: `Bearer ${apiKey}`");
    expect(source).not.toContain("process.env.AGENTOS_API_KEY");
    expect(source).not.toContain("console.log(apiKey)");
  });
});
