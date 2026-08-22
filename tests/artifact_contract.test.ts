import { describe, expect, it } from "bun:test";
import { lstat, readdir } from "node:fs/promises";

const artifactDirectory = new URL(
  "../docs/builds/10000-salvage-the-five-surviving-agentos-work-items-fr/",
  import.meta.url,
);
const requiredArtifacts = [
  "ATTACK_SURFACE.md",
  "DECISIONS.md",
  "INVARIANTS.md",
  "TRACES.md",
];

describe("build 10000 governed artifact contract", () => {
  it("uses a real directory containing the exact regular UTF-8 artifacts", async () => {
    const directory = await lstat(artifactDirectory);
    expect(directory.isDirectory()).toBe(true);
    expect(directory.isSymbolicLink()).toBe(false);
    expect((await readdir(artifactDirectory)).sort()).toEqual(
      [...requiredArtifacts].sort(),
    );

    const decoder = new TextDecoder("utf-8", { fatal: true });
    for (const filename of requiredArtifacts) {
      const url = new URL(filename, artifactDirectory);
      const metadata = await lstat(url);
      expect(metadata.isFile(), `${filename} must be a regular file`).toBe(true);
      expect(metadata.isSymbolicLink()).toBe(false);

      const bytes = await Bun.file(url).bytes();
      expect(bytes.byteLength, `${filename} must contain at least 200 bytes`).toBeGreaterThanOrEqual(200);
      expect(decoder.decode(bytes), `${filename} must contain a Markdown heading`).toMatch(
        /^#{1,6} +\S/m,
      );
    }
  });

  it("traces every required ISC identifier as a whole token", async () => {
    const traces = await Bun.file(new URL("TRACES.md", artifactDirectory)).text();
    for (const identifier of ["ISC-000", "ISC-001", "ISC-002", "ISC-003", "ISC-004"]) {
      expect(traces.match(new RegExp(`\\b${identifier}\\b`, "g"))?.length).toBeGreaterThanOrEqual(1);
    }
  });
});
