import { afterEach, describe, expect, it } from "bun:test";
import {
  lstat,
  mkdtemp,
  mkdir,
  readdir,
  realpath,
  rm,
  symlink,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const artifactDirectory = new URL(
  "../../docs/builds/10000-salvage-the-five-surviving-agentos-work-items-fr/",
  import.meta.url,
);
const build10003ArtifactDirectory = new URL(
  "../../docs/builds/10003-salvage-the-five-surviving-agentos-work-items-fr/",
  import.meta.url,
);
const build10005ArtifactDirectory = new URL(
  "../../docs/builds/10005-salvage-the-five-surviving-agentos-work-items-fr/",
  import.meta.url,
);
const build10010ArtifactDirectory = new URL(
  "../../docs/builds/10010-reconcile-feat-herdr-omp-fleet-with-main-so-that/",
  import.meta.url,
);
const build10011ArtifactDirectory = new URL(
  "../../docs/builds/10011-reconcile-feat-herdr-omp-fleet-with-main-so-that/",
  import.meta.url,
);
const build10013ArtifactDirectory = new URL(
  "../../docs/builds/10013-reconcile-feat-herdr-omp-fleet-with-main-so-that/",
  import.meta.url,
);
const build10014ArtifactDirectory = new URL(
  "../../docs/builds/10014-provider-adapters-must-not-forward-an-assistant/",
  import.meta.url,
);
const build10008ArtifactDirectory = new URL(
  "../../docs/builds/10008-fix-agentos-up-so-a-failed-worker-identity-query/",
  import.meta.url,
);
const requiredArtifacts = [
  "ATTACK_SURFACE.md",
  "DECISIONS.md",
  "INVARIANTS.md",
  "TRACES.md",
] as const;
const requiredIdentifiers = [
  "ISC-000",
  "ISC-001",
  "ISC-002",
  "ISC-003",
  "ISC-004",
] as const;
const salvageBatchRequiredIdentifiers = [
  ...requiredIdentifiers,
  "ISC-005",
] as const;
const reconciliationRequiredIdentifiers = [
  "ISC-000",
  "ISC-001",
  "ISC-002",
  "ISC-003",
] as const;
const assistantAdapterRequiredIdentifiers = ["ISC-000", "ISC-001", "ISC-002"] as const;
const reconciliationConflictFiles = [
  "README.md",
  "crates/cli/src/bootstrap.rs",
  "crates/cli/src/main.rs",
  "crates/cli/tests/portability.rs",
  "e2e/full-stack.test.ts",
  "scripts/dev-up.sh",
  "tests/artifact_contract.test.ts",
  "workers/agent-core/src/main.rs",
  "workers/context-monitor/src/main.rs",
  "workers/eval/src/main.rs",
  "workers/evolve/src/main.rs",
  "workers/memory/src/main.rs",
  "workers/streaming/src/main.rs",
] as const;
const followUpConflictFiles = [
  "README.md",
  "bun.lock",
  "crates/cli/src/main.rs",
  "crates/cli/tests/portability.rs",
  "e2e/full-stack.test.ts",
  "package-lock.json",
  "scripts/dev-up.test.ts",
  "workers/agent-core/src/main.rs",
  "workers/agent-core/src/types.rs",
  "workers/context-monitor/src/main.rs",
  "workers/eval/src/main.rs",
  "workers/evolve/src/main.rs",
  "workers/llm-router/src/main.rs",
  "workers/memory/src/main.rs",
  "workers/streaming/src/main.rs",
  "tests/artifact_contract.test.ts",
] as const;
const minimumArtifactBytes = 200;
const headingPattern = /^#{1,6} +\S/m;
const utf8Decoder = new TextDecoder("utf-8", { fatal: true });
const utf8Encoder = new TextEncoder();

function markdownHeadings(source: string): string[] {
  return source.match(/^#{1,6} +\S.*$/gm) ?? [];
}

function normalizedBuildHeadings(source: string): string[] {
  return markdownHeadings(source).map((heading) =>
    heading.replace(/^# Build \d+ /, "# Build <number> "),
  );
}

function reconciliationDecision(
  source: string,
  filename: string,
): { side: string; reason: string } | null {
  const prefix = `| \`${filename}\` |`;
  const row = source.split(/\r?\n/).find((line) => line.startsWith(prefix));
  if (!row) return null;

  const columns = row.split("|").map((column) => column.trim());
  if (columns.length !== 5) return null;
  const side = columns[2] ?? "";
  const reason = columns[3] ?? "";
  return side && reason ? { side, reason } : null;
}

type ArtifactFailure =
  | "ARTIFACT_DIRECTORY_INVALID"
  | "ARTIFACT_FILE_MISSING"
  | "ARTIFACT_FILE_NOT_REGULAR"
  | "ARTIFACT_FILE_INVALID_UTF8"
  | "ARTIFACT_FILE_TOO_SHORT"
  | "ARTIFACT_FILE_HEADING_MISSING"
  | "TRACES_ISC_MISSING";

interface Failure {
  code: ArtifactFailure;
  path: string;
}

function inspectArtifactBytes(
  filename: string,
  bytes: Uint8Array,
  identifiers: readonly string[] = requiredIdentifiers,
): Failure[] {
  const failures: Failure[] = [];
  if (bytes.byteLength < minimumArtifactBytes) {
    failures.push({ code: "ARTIFACT_FILE_TOO_SHORT", path: filename });
  }

  let text: string;
  try {
    text = utf8Decoder.decode(bytes);
  } catch {
    failures.push({ code: "ARTIFACT_FILE_INVALID_UTF8", path: filename });
    return failures;
  }

  if (!headingPattern.test(text)) {
    failures.push({ code: "ARTIFACT_FILE_HEADING_MISSING", path: filename });
  }

  if (filename === "TRACES.md") {
    for (const identifier of identifiers) {
      if (!new RegExp(`\\b${identifier}\\b`).test(text)) {
        failures.push({ code: "TRACES_ISC_MISSING", path: identifier });
      }
    }
  }
  return failures;
}

async function inspectArtifactDirectory(
  directory: URL,
  identifiers: readonly string[] = requiredIdentifiers,
): Promise<Failure[]> {
  const directoryPath = fileURLToPath(directory);
  let metadata;
  try {
    metadata = await lstat(resolve(directoryPath));
  } catch {
    return [{ code: "ARTIFACT_DIRECTORY_INVALID", path: directoryPath }];
  }

  if (!metadata.isDirectory() || metadata.isSymbolicLink()) {
    return [{ code: "ARTIFACT_DIRECTORY_INVALID", path: directoryPath }];
  }

  const entries = await readdir(directory);
  const failures: Failure[] = [];
  for (const filename of requiredArtifacts) {
    if (!entries.includes(filename)) {
      failures.push({ code: "ARTIFACT_FILE_MISSING", path: filename });
      continue;
    }

    const file = new URL(filename, directory);
    const fileMetadata = await lstat(file);
    if (!fileMetadata.isFile() || fileMetadata.isSymbolicLink()) {
      failures.push({ code: "ARTIFACT_FILE_NOT_REGULAR", path: filename });
      continue;
    }
    failures.push(
      ...inspectArtifactBytes(filename, await Bun.file(file).bytes(), identifiers),
    );
  }
  return failures;
}

const temporaryDirectories: string[] = [];

async function temporaryDirectory(): Promise<URL> {
  const path = await mkdtemp(join(tmpdir(), "agentos-artifact-contract-"));
  temporaryDirectories.push(path);
  return pathToFileURL(`${path}/`);
}

afterEach(async () => {
  await Promise.all(
    temporaryDirectories.splice(0).map((path) => rm(path, { recursive: true, force: true })),
  );
});

describe("build 10000 governed artifact contract", () => {
  it("uses the canonical real directory with exactly the governed files", async () => {
    expect(await realpath(artifactDirectory)).toBe(resolve(fileURLToPath(artifactDirectory)));
    expect((await readdir(artifactDirectory)).sort()).toEqual(
      [...requiredArtifacts].sort(),
    );
  });

  it("accepts every required regular UTF-8 artifact and ISC trace token", async () => {
    expect(await inspectArtifactDirectory(artifactDirectory)).toEqual([]);
  });
});

describe("build 10003 governed artifact contract", () => {
  it("uses the canonical real directory with exactly the governed files", async () => {
    expect(await realpath(build10003ArtifactDirectory)).toBe(
      resolve(fileURLToPath(build10003ArtifactDirectory)),
    );
    expect((await readdir(build10003ArtifactDirectory)).sort()).toEqual(
      [...requiredArtifacts].sort(),
    );
  });

  it("accepts every required regular UTF-8 artifact and ISC-000 through ISC-005", async () => {
    expect(
      await inspectArtifactDirectory(
        build10003ArtifactDirectory,
        salvageBatchRequiredIdentifiers,
      ),
    ).toEqual([]);
  });
});

describe("build 10005 governed artifact contract", () => {
  it("uses the canonical real directory with exactly the governed files", async () => {
    expect(await realpath(build10005ArtifactDirectory)).toBe(
      resolve(fileURLToPath(build10005ArtifactDirectory)),
    );
    expect((await readdir(build10005ArtifactDirectory)).sort()).toEqual(
      [...requiredArtifacts].sort(),
    );
  });

  it("accepts every required regular UTF-8 artifact and ISC-000 through ISC-005", async () => {
    expect(
      await inspectArtifactDirectory(
        build10005ArtifactDirectory,
        salvageBatchRequiredIdentifiers,
      ),
    ).toEqual([]);
  });

  it("preserves build 10000's heading topology in every governed artifact", async () => {
    for (const filename of requiredArtifacts) {
      const build10000 = await Bun.file(new URL(filename, artifactDirectory)).text();
      const build10005 = await Bun.file(
        new URL(filename, build10005ArtifactDirectory),
      ).text();

      expect(normalizedBuildHeadings(build10005), filename).toEqual(
        normalizedBuildHeadings(build10000),
      );
    }
  });

  it("records every trace identifier exactly once and the frozen install decision", async () => {
    const traces = await Bun.file(
      new URL("TRACES.md", build10005ArtifactDirectory),
    ).text();
    for (const identifier of salvageBatchRequiredIdentifiers) {
      expect(
        traces.match(new RegExp(`\\b${identifier}\\b`, "g"))?.length ?? 0,
        identifier,
      ).toBe(1);
    }

    const decisions = await Bun.file(
      new URL("DECISIONS.md", build10005ArtifactDirectory),
    ).text();
    expect(decisions).toContain("bun install --frozen-lockfile");
  });
});

describe("build 10010 reconciliation artifact contract", () => {
  it("uses the canonical real directory with exactly the governed files", async () => {
    expect(await realpath(build10010ArtifactDirectory)).toBe(
      resolve(fileURLToPath(build10010ArtifactDirectory)),
    );
    expect((await readdir(build10010ArtifactDirectory)).sort()).toEqual(
      [...requiredArtifacts].sort(),
    );
  });

  it("accepts every artifact and ISC-000 through ISC-003 as whole tokens", async () => {
    expect(
      await inspectArtifactDirectory(
        build10010ArtifactDirectory,
        reconciliationRequiredIdentifiers,
      ),
    ).toEqual([]);

    const traces = await Bun.file(
      new URL("TRACES.md", build10010ArtifactDirectory),
    ).text();
    for (const identifier of reconciliationRequiredIdentifiers) {
      expect(
        traces.match(new RegExp(`\\b${identifier}\\b`, "g"))?.length ?? 0,
        identifier,
      ).toBe(1);
    }
  });

  it("records the chosen side and reason for every conflicting file", async () => {
    const decisions = await Bun.file(
      new URL("DECISIONS.md", build10010ArtifactDirectory),
    ).text();
    for (const filename of reconciliationConflictFiles) {
      const decision = reconciliationDecision(decisions, filename);
      expect(decision, filename).not.toBeNull();
      expect(decision?.side.length, `${filename} has no chosen side`).toBeGreaterThan(0);
      expect(decision?.reason.length, `${filename} has no reason`).toBeGreaterThan(0);
    }
  });
});

describe("build 10011 follow-up reconciliation artifact contract", () => {
  it("uses the canonical real directory with exactly the four governed files", async () => {
    expect(await realpath(build10011ArtifactDirectory)).toBe(
      resolve(fileURLToPath(build10011ArtifactDirectory)),
    );
    expect((await readdir(build10011ArtifactDirectory)).sort()).toEqual(
      [...requiredArtifacts].sort(),
    );
  });

  it("accepts every artifact and ISC-000 through ISC-003 as whole tokens", async () => {
    expect(
      await inspectArtifactDirectory(
        build10011ArtifactDirectory,
        reconciliationRequiredIdentifiers,
      ),
    ).toEqual([]);

    const traces = await Bun.file(
      new URL("TRACES.md", build10011ArtifactDirectory),
    ).text();
    for (const identifier of reconciliationRequiredIdentifiers) {
      expect(
        traces.match(new RegExp(`\\b${identifier}\\b`, "g"))?.length ?? 0,
        identifier,
      ).toBe(1);
    }
  });

  it("records a non-empty chosen side and reason for every conflicting file", async () => {
    const decisions = await Bun.file(
      new URL("DECISIONS.md", build10011ArtifactDirectory),
    ).text();
    for (const filename of followUpConflictFiles) {
      const decision = reconciliationDecision(decisions, filename);
      expect(decision, filename).not.toBeNull();
      expect(decision?.side.length, `${filename} has no chosen side`).toBeGreaterThan(0);
      expect(decision?.reason.length, `${filename} has no reason`).toBeGreaterThan(0);
    }
  });
});

describe("build 10013 follow-up 2 reconciliation artifact contract", () => {
  it("uses the canonical real directory with exactly the four governed files", async () => {
    expect(await realpath(build10013ArtifactDirectory)).toBe(
      resolve(fileURLToPath(build10013ArtifactDirectory)),
    );
    expect((await readdir(build10013ArtifactDirectory)).sort()).toEqual(
      [...requiredArtifacts].sort(),
    );
  });

  it("accepts every artifact and ISC-000 through ISC-003 as whole tokens", async () => {
    expect(
      await inspectArtifactDirectory(
        build10013ArtifactDirectory,
        reconciliationRequiredIdentifiers,
      ),
    ).toEqual([]);

    const traces = await Bun.file(
      new URL("TRACES.md", build10013ArtifactDirectory),
    ).text();
    for (const identifier of reconciliationRequiredIdentifiers) {
      expect(
        traces.match(new RegExp(`\\b${identifier}\\b`, "g"))?.length ?? 0,
        identifier,
      ).toBe(1);
    }
  });

  it("records a non-empty chosen side and reason for every conflicting file", async () => {
    const decisions = await Bun.file(
      new URL("DECISIONS.md", build10013ArtifactDirectory),
    ).text();
    for (const filename of followUpConflictFiles) {
      const decision = reconciliationDecision(decisions, filename);
      expect(decision, filename).not.toBeNull();
      expect(decision?.side.length, `${filename} has no chosen side`).toBeGreaterThan(0);
      expect(decision?.reason.length, `${filename} has no reason`).toBeGreaterThan(0);
    }
  });
});

describe("build 10014 provider adapter artifact contract", () => {
  it("uses the canonical real directory with exactly the four governed files", async () => {
    expect(await realpath(build10014ArtifactDirectory)).toBe(
      resolve(fileURLToPath(build10014ArtifactDirectory)),
    );
    expect((await readdir(build10014ArtifactDirectory)).sort()).toEqual(
      [...requiredArtifacts].sort(),
    );
  });

  it("accepts every artifact and ISC-000 through ISC-002 as whole tokens", async () => {
    expect(
      await inspectArtifactDirectory(
        build10014ArtifactDirectory,
        assistantAdapterRequiredIdentifiers,
      ),
    ).toEqual([]);

    const traces = await Bun.file(
      new URL("TRACES.md", build10014ArtifactDirectory),
    ).text();
    for (const identifier of assistantAdapterRequiredIdentifiers) {
      expect(new RegExp(`\\b${identifier}\\b`).test(traces), identifier).toBe(true);
    }
  });

  it("states the adapter-scoped history filtering boundary", async () => {
    const attackSurface = await Bun.file(
      new URL("ATTACK_SURFACE.md", build10014ArtifactDirectory),
    ).text();
    expect(attackSurface).toContain("not a global history filter");
    expect(attackSurface).toContain("openai_messages");
    expect(attackSurface).toContain("anthropic_messages");
    expect(attackSurface).toContain("persisted sessions");
    expect(attackSurface).toContain("Gemini");
  });
});
describe("build 10008 worker-identity hardening evidence", () => {
  it("contains exactly the four governed artifacts", async () => {
    expect((await readdir(build10008ArtifactDirectory)).sort()).toEqual(
      [...requiredArtifacts].sort(),
    );
    expect(
      await inspectArtifactDirectory(build10008ArtifactDirectory, [
        "ISC-000",
        "ISC-001",
        "ISC-002",
      ]),
    ).toEqual([]);
  });

  it("records the duplicate-registration path and fail-closed behavior", async () => {
    const attackSurface = await Bun.file(
      new URL("ATTACK_SURFACE.md", build10008ArtifactDirectory),
    ).text();
    expect(attackSurface).toContain("Duplicate-registration path");
    expect(attackSurface).toContain("fails closed");
    expect(attackSurface).toContain("does not spawn any worker");
  });

  it("traces ISC-000 through ISC-002 as whole tokens", async () => {
    const traces = await Bun.file(
      new URL("TRACES.md", build10008ArtifactDirectory),
    ).text();
    for (const identifier of ["ISC-000", "ISC-001", "ISC-002"] as const) {
      expect(new RegExp(`\\b${identifier}\\b`).test(traces), identifier).toBe(true);
    }
  });
});

describe("artifact contract edge cases", () => {
  it("rejects absent, malformed, and empty reconciliation decisions", () => {
    expect(reconciliationDecision("", "README.md")).toBeNull();
    expect(reconciliationDecision("| `README.md` |", "README.md")).toBeNull();
    expect(
      reconciliationDecision("| `README.md` |  | because |", "README.md"),
    ).toBeNull();
    expect(
      reconciliationDecision("| `README.md` | Combined |  |", "README.md"),
    ).toBeNull();
    expect(
      reconciliationDecision(
        "| `README.md` | Combined | preserves both lines |",
        "README.md",
      ),
    ).toEqual({ side: "Combined", reason: "preserves both lines" });
  });

  it("rejects missing, non-directory, and symlinked artifact directories", async () => {
    const parent = await temporaryDirectory();
    const missing = new URL("missing/", parent);
    expect(await inspectArtifactDirectory(missing)).toEqual([
      { code: "ARTIFACT_DIRECTORY_INVALID", path: fileURLToPath(missing) },
    ]);

    const regularFile = new URL("not-a-directory", parent);
    await writeFile(regularFile, "not a directory");
    expect((await inspectArtifactDirectory(regularFile))[0]?.code).toBe(
      "ARTIFACT_DIRECTORY_INVALID",
    );

    const realDirectory = new URL("real/", parent);
    const linkedDirectoryPath = join(fileURLToPath(parent), "linked");
    const linkedDirectory = pathToFileURL(`${linkedDirectoryPath}/`);
    await mkdir(realDirectory);
    await symlink(fileURLToPath(realDirectory), linkedDirectoryPath);
    expect((await inspectArtifactDirectory(linkedDirectory))[0]?.code).toBe(
      "ARTIFACT_DIRECTORY_INVALID",
    );
  });

  it("reports every absent artifact for an empty directory", async () => {
    const empty = await temporaryDirectory();
    expect(await inspectArtifactDirectory(empty)).toEqual(
      requiredArtifacts.map((path) => ({ code: "ARTIFACT_FILE_MISSING", path })),
    );
  });

  it("rejects directories and symbolic links in place of regular artifacts", async () => {
    const directory = await temporaryDirectory();
    await mkdir(new URL("ATTACK_SURFACE.md/", directory));
    await writeFile(new URL("target.md", directory), "# Target\n");
    await symlink("target.md", fileURLToPath(new URL("DECISIONS.md", directory)));

    const failures = await inspectArtifactDirectory(directory);
    expect(failures).toContainEqual({
      code: "ARTIFACT_FILE_NOT_REGULAR",
      path: "ATTACK_SURFACE.md",
    });
    expect(failures).toContainEqual({
      code: "ARTIFACT_FILE_NOT_REGULAR",
      path: "DECISIONS.md",
    });
  });

  it("enforces the 200-byte boundary and handles empty content", () => {
    const exactlyMinimum = utf8Encoder.encode(`# Heading\n${"x".repeat(190)}`);
    expect(exactlyMinimum.byteLength).toBe(minimumArtifactBytes);
    expect(inspectArtifactBytes("ATTACK_SURFACE.md", exactlyMinimum)).toEqual([]);

    const oneByteShort = utf8Encoder.encode(`# Heading\n${"x".repeat(189)}`);
    expect(inspectArtifactBytes("ATTACK_SURFACE.md", oneByteShort)).toContainEqual({
      code: "ARTIFACT_FILE_TOO_SHORT",
      path: "ATTACK_SURFACE.md",
    });
    expect(inspectArtifactBytes("ATTACK_SURFACE.md", new Uint8Array())).toEqual([
      { code: "ARTIFACT_FILE_TOO_SHORT", path: "ATTACK_SURFACE.md" },
      { code: "ARTIFACT_FILE_HEADING_MISSING", path: "ATTACK_SURFACE.md" },
    ]);
  });

  it("rejects malformed UTF-8 without attempting text checks", () => {
    const invalidUtf8 = new Uint8Array(minimumArtifactBytes).fill(0x78);
    invalidUtf8[0] = 0xff;
    expect(inspectArtifactBytes("ATTACK_SURFACE.md", invalidUtf8)).toEqual([
      { code: "ARTIFACT_FILE_INVALID_UTF8", path: "ATTACK_SURFACE.md" },
    ]);
  });

  it("accepts heading levels one through six and rejects missing or level-seven headings", () => {
    for (let level = 1; level <= 6; level += 1) {
      const content = utf8Encoder.encode(`${"#".repeat(level)} Heading\n${"x".repeat(200)}`);
      expect(inspectArtifactBytes("DECISIONS.md", content)).toEqual([]);
    }

    for (const content of ["plain text", "####### Heading", "#    "]) {
      const padded = utf8Encoder.encode(`${content}\n${"x".repeat(200)}`);
      expect(inspectArtifactBytes("DECISIONS.md", padded)).toContainEqual({
        code: "ARTIFACT_FILE_HEADING_MISSING",
        path: "DECISIONS.md",
      });
    }
  });

  it("extracts headings in document order and normalizes only the build title", () => {
    const source = [
      "# Build 10005 Decisions",
      "body # not a heading",
      "### Later",
      "## Earlier level",
    ].join("\n");

    expect(normalizedBuildHeadings(source)).toEqual([
      "# Build <number> Decisions",
      "### Later",
      "## Earlier level",
    ]);
    expect(normalizedBuildHeadings("plain text")).toEqual([]);
  });

  it("requires ISC-000 through ISC-004 as whole tokens", () => {
    const validTokens = utf8Encoder.encode(
      `# Traces\n${requiredIdentifiers.join(", ")}\n${"x".repeat(200)}`,
    );
    expect(inspectArtifactBytes("TRACES.md", validTokens)).toEqual([]);

    const partialTokens = utf8Encoder.encode(
      `# Traces\n${requiredIdentifiers.map((id) => `x${id}x`).join(" ")}\n${"x".repeat(200)}`,
    );
    expect(inspectArtifactBytes("TRACES.md", partialTokens)).toEqual(
      requiredIdentifiers.map((path) => ({ code: "TRACES_ISC_MISSING", path })),
    );

    const missingBuild10003Boundary = utf8Encoder.encode(
      `# Traces\n${requiredIdentifiers.join(", ")}\n${"x".repeat(200)}`,
    );
    expect(
      inspectArtifactBytes(
        "TRACES.md",
        missingBuild10003Boundary,
        salvageBatchRequiredIdentifiers,
      ),
    ).toEqual([{ code: "TRACES_ISC_MISSING", path: "ISC-005" }]);
  });
});
