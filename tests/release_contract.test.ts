import { afterEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const root = process.env.AGENTOS_RELEASE_CONTRACT_ROOT ?? join(import.meta.dir, "..");
const temporary: string[] = [];

function read(relative: string): string {
  return readFileSync(join(root, relative), "utf8");
}

function json(relative: string): Record<string, unknown> {
  return JSON.parse(read(relative)) as Record<string, unknown>;
}

function productVersion(): string {
  const match = read("Cargo.toml").match(/\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"/m);
  if (!match) throw new Error("Cargo workspace version not found");
  return match[1];
}

function workflowStep(workflow: string, name: string): string {
  const lines = workflow.split("\n");
  const marker = `- name: ${name}`;
  const start = lines.findIndex((line) => line.trim() === marker);
  if (start < 0) throw new Error(`workflow step not found: ${name}`);
  const run = lines.findIndex((line, index) => index > start && line.trim() === "run: |");
  if (run < 0) throw new Error(`run block not found for: ${name}`);
  const indent = lines[run].indexOf("run:");
  const body: string[] = [];
  for (let index = run + 1; index < lines.length; index += 1) {
    const line = lines[index];
    if (line.trim() !== "" && line.search(/\S/) <= indent) break;
    body.push(line.slice(Math.min(line.length, indent + 2)));
  }
  return body.join("\n");
}

function spawnBash(script: string, cwd: string, env: Record<string, string>) {
  return Bun.spawnSync(["bash", "-c", script], {
    cwd,
    env: { ...process.env, ...env },
    stdout: "pipe",
    stderr: "pipe",
  });
}

function fixtureRoot(): string {
  const version = productVersion();
  const directory = mkdtempSync(join(tmpdir(), "agentos-release-contract-"));
  temporary.push(directory);
  for (const path of [
    "target/release",
    "config",
    "agents",
    "hands",
    "identity",
    "integrations",
    "plugin",
    "workflows",
    "workers/agent-core",
    "workers/embedding",
  ]) mkdirSync(join(directory, path), { recursive: true });

  for (const binary of ["agentos", "agentos-tui", "agentos-bus-authd", "agentos-core"]) {
    const path = join(directory, "target/release", binary);
    writeFileSync(path, `#!/bin/sh\necho ${binary}\n`);
    chmodSync(path, 0o755);
  }
  writeFileSync(join(directory, "Cargo.toml"), `[workspace.package]\nversion = "${version}"\n`);
  writeFileSync(join(directory, ".iii-version"), "0.22.1\n");
  writeFileSync(join(directory, "config.yaml"), "workers: []\n");
  writeFileSync(join(directory, "iii.lock"), "version: 1\n");
  writeFileSync(join(directory, ".env.example"), "AGENTOS_API_KEY=\n");
  writeFileSync(join(directory, "workers/env.allowlist"), "agent-core=III_URL,AGENTOS_API_KEY\n");
  writeFileSync(join(directory, "workers/agent-core/iii.worker.yaml"), "iii: v1\nname: agent-core\nruntime:\n  kind: rust\nscripts:\n  start: agentos-core\n");
  writeFileSync(join(directory, "workers/agent-core/Cargo.toml"), '[package]\nname = "agentos-core"\nversion.workspace = true\n');
  writeFileSync(join(directory, "workers/embedding/iii.worker.yaml"), "iii: v1\nname: embedding\nruntime:\n  kind: python\nscripts:\n  start: python main.py\n");
  writeFileSync(join(directory, "workers/embedding/main.py"), "print('fixture')\n");
  writeFileSync(join(directory, "workers/embedding/pyproject.toml"), `[project]\nname = "agentos-embedding"\nversion = "${version}"\n`);
  writeFileSync(join(directory, "workers/embedding/uv.lock"), "version = 1\n");
  return directory;
}

afterEach(() => {
  for (const directory of temporary.splice(0)) rmSync(directory, { recursive: true, force: true });
});

describe("release version contract", () => {
  test("all authoritative product metadata follows the Rust workspace version", () => {
    const workspace = read("Cargo.toml").match(/\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"/m);
    if (!workspace) throw new Error("Cargo workspace version not found");
    const version = workspace[1];
    const agentosPackages = [...read("Cargo.lock").matchAll(/\[\[package\]\]\nname = "(agentos[^"]*)"\nversion = "([^"]+)"/g)]
      .map((match) => ({ name: match[1], version: match[2] }));

    expect(agentosPackages.length).toBeGreaterThan(60);
    expect(new Set(agentosPackages.map(({ version: packageVersion }) => packageVersion))).toEqual(new Set([version]));
    expect(json("package.json").version).toBe(version);
    expect(json("plugin/.claude-plugin/plugin.json").version).toBe(version);
    expect(json("website/package.json").version).toBe(version);
    expect(json("website/package-lock.json").version).toBe(version);
    expect((json("website/package-lock.json").packages as Record<string, { version: string }>)[""].version).toBe(version);

    const python = read("workers/embedding/pyproject.toml").match(/\[project\][\s\S]*?^version\s*=\s*"([^"]+)"/m);
    const embeddingLock = read("workers/embedding/uv.lock").match(/\[\[package\]\]\nname = "agentos-embedding"\nversion = "([^"]+)"/);
    expect(python?.[1]).toBe(version);
    expect(embeddingLock?.[1]).toBe(version);
    expect(read("README.md")).toContain(`| agentos | \`${version}\``);
    expect(read("ARCHITECTURE.md")).toContain(`agentos workspace: **${version}**`);
    expect(read("website/components/Footer.tsx")).toContain(`v${version}`);
  });

  test("the release tag guard executes and rejects a tag that differs from Cargo.toml", () => {
    const workflow = read(".github/workflows/release.yml");
    const guard = workflowStep(workflow, "verify release tag matches product version");
    const fixture = fixtureRoot();
    const version = productVersion();

    const matching = spawnBash(guard, fixture, { VERSION: `v${version}` });
    expect(matching.exitCode, matching.stderr.toString()).toBe(0);
    const mismatch = spawnBash(guard, fixture, { VERSION: "v9.9.9" });
    expect(mismatch.exitCode).not.toBe(0);
    expect(mismatch.stderr.toString()).toContain("does not match");
  });
});

describe("release artifact contract", () => {
  test("the real staging shell packages the launch-critical runtime inputs", () => {
    const workflow = read(".github/workflows/release.yml");
    const stage = workflowStep(workflow, "stage bundle");
    const fixture = fixtureRoot();
    const githubEnv = join(fixture, "github.env");
    writeFileSync(githubEnv, "");

    const version = productVersion();
    const result = spawnBash(stage, fixture, {
      VERSION: `v${version}`,
      TARGET: "x86_64-linux",
      GITHUB_ENV: githubEnv,
    });
    expect(result.exitCode, result.stderr.toString()).toBe(0);

    const archive = join(fixture, `dist/agentos-${version}-x86_64-linux.tar.gz`);
    const listing = Bun.spawnSync(["tar", "-tzf", archive], { stdout: "pipe", stderr: "pipe" });
    expect(listing.exitCode, listing.stderr.toString()).toBe(0);
    const contents = new Set(listing.stdout.toString().trim().split("\n"));
    for (const required of [
      "./bin/agentos",
      "./bin/agentos-tui",
      "./bin/agentos-bus-authd",
      "./runtime/iii.lock",
      "./runtime/.env.example",
      "./runtime/workers/env.allowlist",
      "./runtime/workers/agent-core/iii.worker.yaml",
      "./runtime/target/release/agentos-agent-core",
    ]) expect(contents.has(required), `bundle missing ${required}`).toBe(true);
  });

  test("validate checks every launch-critical file and publish waits for both gates", () => {
    const workflow = read(".github/workflows/release.yml");
    const validate = workflowStep(workflow, "inspect isolated bundle");
    for (const required of [
      "./bin/agentos-bus-authd",
      "./runtime/iii.lock",
      "./runtime/.env.example",
      "./runtime/workers/env.allowlist",
    ]) expect(validate).toContain(required);
    for (const source of [".env.example", "iii.lock", "workers/env.allowlist"]) {
      expect(validate).toContain(`cmp -s ${source}`);
    }

    expect(workflow).toMatch(/publish:\s[\s\S]*?needs:\s*\[build, validate\]/);
    expect(workflow).toContain("anchore/sbom-action@");
    expect(workflow).toContain("actions/attest-build-provenance@");
    expect(workflow).toContain("dist/*.sha256");
    expect(workflow).toMatch(/concurrency:\s[\s\S]*?cancel-in-progress:\s*false/);
  });

  test("portable CI executes the same launch-critical package shape after env-policy integration", () => {
    if (!Bun.file(join(root, "workers/env.allowlist")).size) return;
    const ci = read(".github/workflows/ci.yml");
    const stage = workflowStep(ci, "stage and inspect isolated bundle");
    const fixture = fixtureRoot();
    const runnerTemp = join(fixture, "runner-temp");
    mkdirSync(runnerTemp);
    const result = spawnBash(stage, fixture, {
      TARGET_OS: "linux",
      TARGET_ARCH: "x86_64",
      RUNNER_TEMP: runnerTemp,
      HOME: join(fixture, "home"),
      AGENTOS_HOME: join(fixture, "home/.agentos"),
      BIN_DIR: join(fixture, "bin"),
    });
    expect(result.exitCode, result.stderr.toString()).toBe(0);
    expect(stage).not.toContain('version="0.1.0"');

    const archives = [...new Bun.Glob("agentos-*.tar.gz").scanSync(runnerTemp)];
    expect(archives).toHaveLength(1);
    const listing = Bun.spawnSync(["tar", "-tzf", join(runnerTemp, archives[0]!)], { stdout: "pipe", stderr: "pipe" });
    expect(listing.exitCode, listing.stderr.toString()).toBe(0);
    const contents = new Set(listing.stdout.toString().trim().split("\n"));
    for (const required of [
      "./bin/agentos-bus-authd",
      "./runtime/iii.lock",
      "./runtime/.env.example",
      "./runtime/workers/env.allowlist",
    ]) expect(contents.has(required), `portable bundle missing ${required}`).toBe(true);
  });
});

describe("public setup documentation", () => {
  test("website uses the guarded launcher and never starts a worker wildcard", () => {
    const install = read("website/components/Install.tsx");
    expect(install).toContain("agentos up");
    expect(install).toContain(".env.example");
    expect(install).not.toMatch(/for w in|agentos-\*/);
    expect(install).not.toContain("iii --config config.yaml");
  });

  test("documents Linux detached lifecycle separately from macOS foreground lifecycle", () => {
    const readme = read("README.md");
    const installGuide = read("INSTALL_STACK.md");
    const website = read("website/components/Install.tsx");
    for (const document of [readme, installGuide, website]) {
      expect(document).toContain("Linux");
      expect(document).toContain("macOS");
      expect(document).toContain("agentos up");
      expect(document).toContain("agentos start");
      expect(document).toContain("agentos tui");
      expect(document).toContain("runtime proof");
    }
    expect(website).toContain("--grace-seconds");
    expect(readme).toContain("--grace-seconds");
    expect(readme).toContain("0..60");
    expect(readme).toContain("default 5");
    expect(readme).toContain("runtime proof");
  });

  test("documents configured UTC pulse slots instead of a one-minute throttle", () => {
    const architecture = read("ARCHITECTURE.md");
    expect(architecture).toContain("5- or 6-field UTC");
    expect(architecture).toContain("five-second late-delivery window");
    expect(architecture).toContain("Older due slots are not caught up");
    expect(architecture).not.toContain("capped at one per minute");
  });

  test("root security, release, and contribution entry points exist", () => {
    for (const path of ["SECURITY.md", "RELEASING.md", "CONTRIBUTING.md"]) {
      expect(Bun.file(join(root, path)).size, `${path} is empty or absent`).toBeGreaterThan(0);
    }
    expect(read("CONTRIBUTING.md")).toContain("identity/CONTRIBUTING.md");
  });
});
