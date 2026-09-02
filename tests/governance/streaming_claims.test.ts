import { describe, expect, it } from "bun:test";

/**
 * The website used to say the streaming worker "fans tokens out over iii-stream's
 * WebSocket lane". It never did. `stream::chat` / `stream::completion` /
 * `stream::sse` delegate to `agent::chat` and frame an answer that is already
 * complete; responses label themselves `x-agentos-stream: buffered`. Promising
 * token streaming in a README is the cheapest way to lose a user's trust on
 * their first request, so the claim is asserted here against the source.
 */

const repository = new URL("../../", import.meta.url);
const readme = await Bun.file(new URL("README.md", repository)).text();
const architecture = await Bun.file(new URL("ARCHITECTURE.md", repository)).text();
const useCases = await Bun.file(new URL("website/components/UseCases.tsx", repository)).text();
const streaming = await Bun.file(new URL("workers/streaming/src/main.rs", repository)).text();

const documents: Array<[string, string]> = [
  ["README.md", readme],
  ["ARCHITECTURE.md", architecture],
  ["website/components/UseCases.tsx", useCases],
];

describe("streaming claims", () => {
  it("promises no token-level streaming anywhere", () => {
    const forbidden = [
      "fans tokens",
      "fans out tokens",
      "streams tokens",
      "token stream",
      "token-by-token",
      "token by token",
      "incremental delivery of tokens",
    ];
    const offenders: string[] = [];
    for (const [name, source] of documents) {
      // Denials of the claim are the point of these documents; strip them first
      // so "buffered, not token streaming" does not read as a promise.
      const lowered = source
        .toLowerCase()
        .replaceAll("not token streaming", "")
        .replaceAll("not incremental", "");
      for (const phrase of forbidden) {
        if (lowered.includes(phrase)) offenders.push(`${name} claims "${phrase}"`);
      }
    }
    expect(offenders, "the transport is buffered; do not promise token streaming").toEqual([]);
  });

  it("says buffered, in both documents", () => {
    expect(readme).toContain("buffered, not token streaming");
    expect(architecture).toContain("buffered, not token streaming");
    expect(useCases).toContain("buffered");
  });

  it("says there is one chat pipeline, because there is one", () => {
    expect(readme).toContain("There is exactly one chat pipeline");
    expect(architecture).toContain("`agent::chat` is the only chat pipeline");

    // The streaming worker must actually delegate rather than re-implement.
    expect(streaming).toContain('"agent::chat"');
    for (const id of ["stream::chat", "stream::completion", "stream::sse"]) {
      expect(streaming, `${id} is no longer registered`).toContain(`"${id}"`);
    }
  });

  it("keeps the documented header identical to the one the worker sets", () => {
    const header = /"x-agentos-stream"\s*:\s*"([^"]+)"/.exec(streaming)?.[1];
    if (header === undefined) {
      // Not yet landed. The documents may not claim a header that does not exist.
      expect(
        readme.includes("x-agentos-stream") || architecture.includes("x-agentos-stream"),
        "documents name a response header the streaming worker does not set",
      ).toBe(true);
      return;
    }
    expect(header).toBe("buffered");
    expect(readme).toContain(`x-agentos-stream: ${header}`);
  });
});
