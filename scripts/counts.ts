#!/usr/bin/env bun
/**
 * scripts/counts.ts — the single source of truth for every number this repository publishes.
 *
 *   bun scripts/counts.ts            # print what the tree actually contains
 *   bun scripts/counts.ts --json     # same, machine readable
 *   bun scripts/counts.ts --check    # exit 1 when a published number has drifted
 *   bun scripts/counts.ts --write    # rewrite every published number from the tree
 *
 * Nothing here is hand maintained. Every value is derived from source: worker
 * manifests, `register_function(` call sites, `#[test]` attributes, the
 * llm-router provider table, the CLI command enum, the TUI screen enum,
 * `config.yaml`, `.github/workflows/ci.yml` and `.github/workflows/release.yml`.
 *
 * `--check` runs in CI (job `node-unit`) and is also asserted by
 * `tests/counts_contract.test.ts`, so a stale README fails a pull request.
 */

import { readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

export const repositoryRoot = fileURLToPath(new URL("../", import.meta.url));

const SOURCE_ROOTS = ["workers", "crates"] as const;
const SKIP_DIRECTORIES = new Set([
  ".git",
  ".worktrees",
  "node_modules",
  "target",
  "dist",
  "__pycache__",
]);

/* ------------------------------------------------------------------ helpers */

function read(relativePath: string): string {
  return readFileSync(join(repositoryRoot, relativePath), "utf8");
}

function listRustSources(): string[] {
  const found: string[] = [];
  const walk = (relativeDirectory: string): void => {
    for (const entry of readdirSync(join(repositoryRoot, relativeDirectory))) {
      if (SKIP_DIRECTORIES.has(entry)) continue;
      const relativePath = join(relativeDirectory, entry);
      if (statSync(join(repositoryRoot, relativePath)).isDirectory()) walk(relativePath);
      else if (entry.endsWith(".rs")) found.push(relativePath);
    }
  };
  for (const root of SOURCE_ROOTS) walk(root);
  return found.sort();
}

function countMatches(haystack: string, pattern: RegExp): number {
  return haystack.match(pattern)?.length ?? 0;
}

/**
 * Blank out every `#[cfg(test)] mod ... { ... }` block, preserving byte offsets
 * and line numbers. Registrations and routes are a property of the shipped
 * binary; a `json!` literal inside a unit test is not one.
 */
export function withoutTestModules(source: string): string {
  let result = source;
  for (;;) {
    const attribute = result.indexOf("#[cfg(test)]");
    if (attribute < 0) break;
    const open = result.indexOf("{", attribute);
    const terminator = result.indexOf(";", attribute);
    if (open < 0 || (terminator >= 0 && terminator < open)) {
      // `#[cfg(test)] use ...;` and friends annotate a non-block item. Blank the
      // attribute only, so the scan advances without eating real source.
      result =
        result.slice(0, attribute) + " ".repeat("#[cfg(test)]".length) + result.slice(attribute + "#[cfg(test)]".length);
      continue;
    }
    let depth = 0;
    let close = -1;
    for (let index = open; index < result.length; index += 1) {
      const character = result[index];
      if (character === "{") depth += 1;
      else if (character === "}") {
        depth -= 1;
        if (depth === 0) {
          close = index;
          break;
        }
      }
    }
    if (close < 0) close = result.length - 1;
    const blanked = result
      .slice(attribute, close + 1)
      .replace(/[^\n]/g, " ");
    result = result.slice(0, attribute) + blanked + result.slice(close + 1);
  }
  return result;
}

/** Line number (1-based) of a byte offset, for human-readable drift reports. */
function lineOf(text: string, index: number): number {
  return text.slice(0, index).split("\n").length;
}

const NUMBER_WORDS = [
  "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
  "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen",
  "seventeen", "eighteen", "nineteen", "twenty",
];

export function numberWord(value: number): string {
  const word = NUMBER_WORDS[value];
  if (word === undefined) throw new Error(`no english word for ${value}`);
  return word;
}

export function grouped(value: number): string {
  return value.toLocaleString("en-US");
}

/* -------------------------------------------------------------- collection */

export interface WorkerRecord {
  readonly name: string;
  readonly runtime: string;
}

export interface RegistrationSite {
  readonly id: string;
  readonly file: string;
  readonly line: number;
}

export interface RouteSite {
  readonly route: string;
  readonly file: string;
  readonly line: number;
}

export interface Counts {
  readonly workers: WorkerRecord[];
  readonly workerCount: number;
  readonly rustWorkerCount: number;
  readonly pythonWorkerCount: number;
  readonly functionRegistrations: RegistrationSite[];
  readonly functionRegistrationCount: number;
  readonly functionIdCount: number;
  readonly duplicateFunctionIds: Map<string, RegistrationSite[]>;
  readonly httpRoutes: RouteSite[];
  readonly duplicateHttpRoutes: Map<string, RouteSite[]>;
  readonly rustTestAttributes: number;
  readonly ignoredRustTests: number;
  readonly providers: number;
  readonly cliSubcommands: string[];
  readonly tuiScreens: string[];
  readonly ciJobs: string[];
  readonly engineWorkers: string[];
  readonly engineConfigFiles: string[];
  readonly releaseTargets: string[];
  readonly repositorySlug: string;
}

/** Every `workers/<name>/iii.worker.yaml`, with the runtime it declares. */
export function collectWorkers(): WorkerRecord[] {
  const workers: WorkerRecord[] = [];
  for (const entry of readdirSync(join(repositoryRoot, "workers")).sort()) {
    const manifest = join("workers", entry, "iii.worker.yaml");
    let text: string;
    try {
      text = read(manifest);
    } catch {
      continue;
    }
    const runtime = /^\s{2}kind:\s*(\S+)\s*$/m.exec(text)?.[1];
    if (runtime === undefined) throw new Error(`${manifest} declares no runtime.kind`);
    workers.push({ name: entry, runtime });
  }
  return workers;
}

/**
 * Literal `register_function("id", ...)` call sites. Two further call sites build
 * their id at runtime (`workers/hand-runner` per hand, `crates/http-adapter` per
 * adapter) and are deliberately not counted as declared ids.
 */
export function collectRegistrations(): RegistrationSite[] {
  const sites: RegistrationSite[] = [];
  for (const file of listRustSources()) {
    const text = withoutTestModules(read(file));
    for (const match of text.matchAll(/register_function\(\s*"([^"]+)"/g)) {
      sites.push({ id: match[1]!, file, line: lineOf(text, match.index) });
    }
  }
  return sites;
}

/** `"<METHOD> <api_path>"` for every literal HTTP trigger registered by a worker. */
export function collectHttpRoutes(): RouteSite[] {
  const routes: RouteSite[] = [];
  for (const file of listRustSources()) {
    const text = withoutTestModules(read(file));
    for (const match of text.matchAll(/json!\(\{[^}]*"api_path"[^}]*\}\)/g)) {
      const body = match[0];
      const path = /"api_path"\s*:\s*"([^"]*)"/.exec(body)?.[1];
      const method = /"http_method"\s*:\s*"([^"]*)"/.exec(body)?.[1];
      if (path === undefined || method === undefined) continue;
      const normalised = `${method.toUpperCase()} /${path.replace(/^\/+/, "")}`;
      routes.push({ route: normalised, file, line: lineOf(text, match.index) });
    }
  }
  return routes;
}

function groupBy<T>(items: T[], key: (item: T) => string): Map<string, T[]> {
  const grouping = new Map<string, T[]>();
  for (const item of items) {
    const bucket = grouping.get(key(item));
    if (bucket) bucket.push(item);
    else grouping.set(key(item), [item]);
  }
  return grouping;
}

function duplicatesOf<T>(grouping: Map<string, T[]>): Map<string, T[]> {
  return new Map([...grouping].filter(([, sites]) => sites.length > 1));
}

function enumVariants(source: string, declaration: string): string[] {
  const start = source.indexOf(declaration);
  if (start < 0) throw new Error(`${declaration} not found`);
  const end = source.indexOf("\n}\n", start);
  if (end < 0) throw new Error(`${declaration} is not terminated`);
  const body = source.slice(start + declaration.length, end);
  return [...body.matchAll(/^ {4}([A-Z][A-Za-z0-9]*)\s*(?:\{|\(|,)/gm)].map((match) => match[1]!);
}

/** Keys under `jobs:` in a workflow file, in declaration order. */
export function workflowJobs(relativePath: string): string[] {
  const lines = read(relativePath).split("\n");
  const start = lines.findIndex((line) => line === "jobs:");
  if (start < 0) throw new Error(`${relativePath} declares no jobs`);
  const jobs: string[] = [];
  for (const line of lines.slice(start + 1)) {
    if (/^\S/.test(line)) break;
    const match = /^ {2}([A-Za-z0-9_-]+):\s*$/.exec(line);
    if (match) jobs.push(match[1]!);
  }
  return jobs;
}

export function collectCounts(): Counts {
  const workers = collectWorkers();
  const functionRegistrations = collectRegistrations();
  const httpRoutes = collectHttpRoutes();
  const byId = groupBy(functionRegistrations, (site) => site.id);
  const byRoute = groupBy(httpRoutes, (site) => site.route);

  let rustTestAttributes = 0;
  let ignoredRustTests = 0;
  for (const file of listRustSources()) {
    const text = read(file);
    rustTestAttributes += countMatches(text, /#\[test\]/g);
    rustTestAttributes += countMatches(text, /#\[tokio::test/g);
    ignoredRustTests += countMatches(text, /#\[ignore/g);
  }

  const router = read("workers/llm-router/src/main.rs");
  const providerTableStart = router.indexOf("fn default_providers()");
  if (providerTableStart < 0) throw new Error("llm-router declares no default_providers()");
  const providerTableEnd = router.indexOf("\n}\n", providerTableStart);
  const providers = countMatches(
    router.slice(providerTableStart, providerTableEnd),
    /\bDriver::[A-Za-z]+,/g,
  );

  const engineWorkers = [...read("config.yaml").matchAll(/^ {2}- name:\s*(\S+)\s*$/gm)].map(
    (match) => match[1]!,
  );
  const engineConfigFiles = readdirSync(join(repositoryRoot, "config"))
    .filter((entry) => entry.endsWith(".yaml"))
    .sort();

  const release = read(".github/workflows/release.yml");
  const buildMatrix = release.slice(
    release.indexOf("  build:"),
    release.indexOf("  validate:"),
  );
  const releaseTargets = [...buildMatrix.matchAll(/^ {10}- runner: \S+\n {12}os: (\S+)\n {12}arch: (\S+)$/gm)].map(
    (match) => `${match[2]!}-${match[1]!}`,
  );

  const repositorySlug = /github\.repository == '([^']+)'/.exec(read(".github/workflows/ci.yml"))?.[1];
  if (repositorySlug === undefined) throw new Error("ci.yml pins no repository slug");

  return {
    workers,
    workerCount: workers.length,
    rustWorkerCount: workers.filter((worker) => worker.runtime === "rust").length,
    pythonWorkerCount: workers.filter((worker) => worker.runtime === "python").length,
    functionRegistrations,
    functionRegistrationCount: functionRegistrations.length,
    functionIdCount: byId.size,
    duplicateFunctionIds: duplicatesOf(byId),
    httpRoutes,
    duplicateHttpRoutes: duplicatesOf(byRoute),
    rustTestAttributes,
    ignoredRustTests,
    providers,
    cliSubcommands: enumVariants(read("crates/cli/src/main.rs"), "enum Commands {"),
    tuiScreens: enumVariants(read("crates/tui/src/main.rs"), "enum Screen {"),
    ciJobs: workflowJobs(".github/workflows/ci.yml"),
    engineWorkers,
    engineConfigFiles,
    releaseTargets,
    repositorySlug,
  };
}

/* ------------------------------------------------------- published numbers */

export interface PublishedNumber {
  readonly file: string;
  readonly label: string;
  /** Must contain exactly one capture group: the published value. */
  readonly pattern: RegExp;
  readonly expected: string;
  /** How many times the pattern must match. Guards against a silent no-op. */
  readonly occurrences: number;
}

export function publishedNumbers(counts: Counts): PublishedNumber[] {
  const workers = String(counts.workerCount);
  const rust = String(counts.rustWorkerCount);
  const functions = String(counts.functionIdCount);
  const registrations = String(counts.functionRegistrationCount);
  const tests = String(counts.rustTestAttributes);

  return [
    // README badges
    { file: "README.md", label: "workers badge", pattern: /badge\/workers-(\d+)-/g, expected: workers, occurrences: 1 },
    { file: "README.md", label: "functions badge", pattern: /badge\/functions-(\d+)-/g, expected: functions, occurrences: 1 },
    { file: "README.md", label: "rust test badge", pattern: /badge\/rust_tests-(\d+)_total-/g, expected: tests, occurrences: 1 },
    { file: "README.md", label: "rust test badge alt text", pattern: /alt="([\d,]+) Rust tests"/g, expected: grouped(counts.rustTestAttributes), occurrences: 1 },
    // README prose
    { file: "README.md", label: "thesis worker count", pattern: /(\d+) narrow workers/g, expected: workers, occurrences: 1 },
    { file: "README.md", label: "thesis rust worker count", pattern: /(\d+) Rust binaries plus one Python worker/g, expected: rust, occurrences: 1 },
    { file: "README.md", label: "quickstart rust worker count", pattern: /starts the (\d+) Rust workers/g, expected: rust, occurrences: 1 },
    { file: "README.md", label: "registration count", pattern: /source registers (\d+) literal function/g, expected: registrations, occurrences: 1 },
    { file: "README.md", label: "distinct function ids", pattern: /(\d+) distinct function ids/g, expected: functions, occurrences: 1 },
    { file: "README.md", label: "worker section subtitle", pattern: /^(\d+) Rust \+ 1 Python, grouped by responsibility\./gm, expected: rust, occurrences: 1 },
    { file: "README.md", label: "layout worker count", pattern: /^workers\/ +(\d+) Rust \+ 1 Python \(embedding\)$/gm, expected: rust, occurrences: 1 },
    { file: "README.md", label: "engine config file count", pattern: /committed values for ([a-z]+) iii worker configurations/g, expected: numberWord(counts.engineConfigFiles.length), occurrences: 1 },
    { file: "README.md", label: "unproven release targets", pattern: /does not prove the other ([a-z]+) release targets/g, expected: numberWord(counts.releaseTargets.length - 1), occurrences: 1 },
    { file: "README.md", label: "release target bundles", pattern: /builds and inspects all ([a-z]+) target bundles/g, expected: numberWord(counts.releaseTargets.length), occurrences: 1 },

    // ARCHITECTURE
    { file: "ARCHITECTURE.md", label: "intro worker count", pattern: /ships \*\*(\d+) narrow workers\*\*/g, expected: workers, occurrences: 1 },
    { file: "ARCHITECTURE.md", label: "intro rust worker count", pattern: /\((\d+) Rust workers and one Python worker\)/g, expected: rust, occurrences: 1 },
    { file: "ARCHITECTURE.md", label: "layout rust worker count", pattern: /workers\/ +(\d+) Rust workers \+ 1 Python worker/g, expected: rust, occurrences: 1 },
    { file: "ARCHITECTURE.md", label: "registration count", pattern: /source registers (\d+) literal function/g, expected: registrations, occurrences: 1 },
    { file: "ARCHITECTURE.md", label: "distinct function ids", pattern: /(\d+) distinct function ids/g, expected: functions, occurrences: 1 },
    { file: "ARCHITECTURE.md", label: "registration worker count", pattern: /across (\d+) workers \(\d+ Rust \+ 1 Python\)/g, expected: workers, occurrences: 1 },
    { file: "ARCHITECTURE.md", label: "registration rust worker count", pattern: /across \d+ workers \((\d+) Rust \+ 1 Python\)/g, expected: rust, occurrences: 1 },
    { file: "ARCHITECTURE.md", label: "engine worker count", pattern: /It declares ([a-z]+) engine workers/g, expected: numberWord(counts.engineWorkers.length), occurrences: 1 },
    { file: "ARCHITECTURE.md", label: "ci job count", pattern: /defines ([a-z]+) jobs/g, expected: numberWord(counts.ciJobs.length), occurrences: 1 },
    { file: "ARCHITECTURE.md", label: "rust test count", pattern: /([\d,]+) test attributes;/g, expected: grouped(counts.rustTestAttributes), occurrences: 1 },
    { file: "ARCHITECTURE.md", label: "ignored rust test count", pattern: /test attributes; (\d+) live-engine checks ignored/g, expected: String(counts.ignoredRustTests), occurrences: 1 },

    // website
    { file: "website/components/Hero.tsx", label: "hero rust worker count", pattern: /(\d+) Rust binaries and one Python worker/g, expected: rust, occurrences: 1 },
    { file: "website/components/Hero.tsx", label: "hero worker count", pattern: /<span>(\d+) workers<\/span>/g, expected: workers, occurrences: 1 },
    { file: "website/components/Hero.tsx", label: "hero function count", pattern: /<span>(\d+) functions<\/span>/g, expected: functions, occurrences: 1 },
    { file: "website/components/Hero.tsx", label: "hero test count", pattern: /<span>(\d+) tests<\/span>/g, expected: tests, occurrences: 1 },
    { file: "website/components/Footer.tsx", label: "footer worker count", pattern: /(\d+) workers · \d+ functions · iii-sdk/g, expected: workers, occurrences: 1 },
    { file: "website/components/Footer.tsx", label: "footer function count", pattern: /\d+ workers · (\d+) functions · iii-sdk/g, expected: functions, occurrences: 1 },

    // plugin manifest
    { file: "plugin/.claude-plugin/plugin.json", label: "tui screen count", pattern: /(\d+)-screen TUI/g, expected: String(counts.tuiScreens.length), occurrences: 1 },
    { file: "plugin/.claude-plugin/plugin.json", label: "provider count", pattern: /(\d+) LLM providers/g, expected: String(counts.providers), occurrences: 1 },
  ];
}

/* ---------------------------------------------------------- worker tables */

const CHANNEL_GROUP = "Channels";

function expandWorkerToken(token: string): string[] {
  const braces = /^([a-z0-9-]*)\{([^}]*)\}$/.exec(token);
  if (!braces) return [token];
  return braces[2]!
    .split(",")
    .map((part) => `${braces[1]!}${part.trim()}`)
    .filter((name) => name.length > braces[1]!.length);
}

function markdownWorkerTable(text: string, heading: string): string[] {
  const start = text.indexOf(heading);
  if (start < 0) throw new Error(`worker table heading not found: ${heading}`);
  const rows = text.slice(start).split("\n");
  const names: string[] = [];
  let seen = false;
  for (const row of rows) {
    if (!row.startsWith("|")) {
      if (seen) break;
      continue;
    }
    seen = true;
    const cells = row.split("|").slice(1, -1);
    if (cells.length < 2) continue;
    for (const token of cells[1]!.matchAll(/`([^`]+)`/g)) {
      const raw = token[1]!.replace(/\s*\(Python\)\s*/i, "").trim();
      if (raw.includes("::")) continue;
      names.push(...expandWorkerToken(raw));
    }
  }
  return names;
}

function countsComponentWorkers(text: string): string[] {
  const start = text.indexOf("const groups = [");
  if (start < 0) throw new Error("website/components/Counts.tsx declares no groups");
  const end = text.indexOf("\n];", start);
  const names: string[] = [];
  for (const entry of text.slice(start, end).matchAll(/\{\s*label:\s*"([^"]+)",\s*workers:\s*\[([^\]]*)\]/g)) {
    const label = entry[1]!;
    for (const worker of entry[2]!.matchAll(/"([^"]+)"/g)) {
      const raw = worker[1]!.replace(/\s*\(python\)\s*/i, "").trim();
      names.push(label === CHANNEL_GROUP ? `channel-${raw}` : raw);
    }
  }
  return names;
}

export interface WorkerTableSite {
  readonly file: string;
  readonly parse: () => string[];
}

/**
 * The engine workers ARCHITECTURE.md enumerates in prose. The count beside them
 * is a published number, but the *names* are free text, so they would otherwise
 * rot silently when config.yaml gains or loses an entry.
 */
export function publishedEngineWorkers(): string[] {
  const text = read("ARCHITECTURE.md");
  const start = text.indexOf("It declares");
  const end = text.indexOf("These are upstream registry binaries", start);
  if (start < 0 || end < 0) throw new Error("ARCHITECTURE.md no longer enumerates the engine workers");
  return [...text.slice(start, end).matchAll(/`([^`]+)`/g)].map((match) => match[1]!);
}

export function workerTableSites(): WorkerTableSite[] {
  return [
    {
      file: "README.md",
      parse: () => markdownWorkerTable(read("README.md"), "\n## § 06 · Workers\n"),
    },
    {
      file: "ARCHITECTURE.md",
      parse: () => markdownWorkerTable(read("ARCHITECTURE.md"), "\n## Workers\n"),
    },
    {
      file: "website/components/Counts.tsx",
      parse: () => countsComponentWorkers(read("website/components/Counts.tsx")),
    },
  ];
}

/* ----------------------------------------------------------------- drift */

export interface Drift {
  readonly file: string;
  readonly label: string;
  readonly found: string;
  readonly expected: string;
  readonly line: number;
  readonly writable: boolean;
}

export function findDrift(counts: Counts): Drift[] {
  const drift: Drift[] = [];

  for (const site of publishedNumbers(counts)) {
    const text = read(site.file);
    const matches = [...text.matchAll(site.pattern)];
    if (matches.length !== site.occurrences) {
      drift.push({
        file: site.file,
        label: site.label,
        found: `${matches.length} occurrence(s) of ${site.pattern.source}`,
        expected: `${site.occurrences} occurrence(s)`,
        line: 0,
        writable: false,
      });
      continue;
    }
    for (const match of matches) {
      if (match[1] === site.expected) continue;
      drift.push({
        file: site.file,
        label: site.label,
        found: match[1] ?? "",
        expected: site.expected,
        line: lineOf(text, match.index),
        writable: true,
      });
    }
  }

  const declaredEngineWorkers = new Set(counts.engineWorkers);
  const publishedEngine = publishedEngineWorkers();
  for (const name of publishedEngine) {
    if (!declaredEngineWorkers.has(name)) {
      drift.push({
        file: "ARCHITECTURE.md",
        label: `engine worker list names "${name}", which config.yaml does not declare`,
        found: "listed",
        expected: "absent",
        line: 0,
        writable: false,
      });
    }
  }
  for (const name of counts.engineWorkers) {
    if (!publishedEngine.includes(name)) {
      drift.push({
        file: "ARCHITECTURE.md",
        label: `engine worker list is missing "${name}"`,
        found: "absent",
        expected: "named in the engine-boot paragraph",
        line: 0,
        writable: false,
      });
    }
  }
  if (new Set(publishedEngine).size !== publishedEngine.length) {
    drift.push({
      file: "ARCHITECTURE.md",
      label: "engine worker list names a worker twice",
      found: "duplicated",
      expected: "named once",
      line: 0,
      writable: false,
    });
  }

  const actual = new Set(counts.workers.map((worker) => worker.name));
  for (const site of workerTableSites()) {
    const published = site.parse();
    const publishedSet = new Set(published);
    for (const name of [...actual].sort()) {
      if (!publishedSet.has(name)) {
        drift.push({
          file: site.file,
          label: `worker table is missing "${name}"`,
          found: "absent",
          expected: "listed in the worker table",
          line: 0,
          writable: false,
        });
      }
    }
    for (const name of [...publishedSet].sort()) {
      if (!actual.has(name)) {
        drift.push({
          file: site.file,
          label: `worker table lists "${name}", which has no workers/<name>/iii.worker.yaml`,
          found: "listed",
          expected: "absent",
          line: 0,
          writable: false,
        });
      }
    }
    if (published.length !== publishedSet.size) {
      const seen = new Set<string>();
      for (const name of published) {
        if (seen.has(name)) {
          drift.push({
            file: site.file,
            label: `worker table lists "${name}" more than once`,
            found: "duplicated",
            expected: "listed once",
            line: 0,
            writable: false,
          });
        }
        seen.add(name);
      }
    }
  }

  return drift;
}

export function applyDrift(counts: Counts): string[] {
  const changed: string[] = [];
  for (const site of publishedNumbers(counts)) {
    const text = read(site.file);
    const matches = [...text.matchAll(site.pattern)];
    if (matches.length !== site.occurrences) continue;
    let updated = text;
    let moved = false;
    for (const match of matches.reverse()) {
      if (match[1] === site.expected) continue;
      const whole = match[0];
      const at = whole.lastIndexOf(match[1]!);
      const replacement = whole.slice(0, at) + site.expected + whole.slice(at + match[1]!.length);
      updated = updated.slice(0, match.index) + replacement + updated.slice(match.index + whole.length);
      moved = true;
    }
    if (moved) {
      writeFileSync(join(repositoryRoot, site.file), updated);
      changed.push(`${site.file}: ${site.label} -> ${site.expected}`);
    }
  }
  return changed;
}

/* ------------------------------------------------------------------- main */

function report(counts: Counts): string {
  const lines = [
    `workers                    ${counts.workerCount} (${counts.rustWorkerCount} rust, ${counts.pythonWorkerCount} python)`,
    `register_function sites    ${counts.functionRegistrationCount} literal`,
    `distinct function ids      ${counts.functionIdCount}`,
    `duplicate function ids     ${counts.duplicateFunctionIds.size}`,
    `http routes                ${counts.httpRoutes.length} literal`,
    `duplicate http routes      ${counts.duplicateHttpRoutes.size}`,
    `rust test attributes       ${counts.rustTestAttributes} (${counts.ignoredRustTests} ignored)`,
    `llm providers              ${counts.providers}`,
    `cli subcommands            ${counts.cliSubcommands.length}`,
    `tui screens                ${counts.tuiScreens.length}`,
    `ci jobs                    ${counts.ciJobs.length}`,
    `engine workers             ${counts.engineWorkers.length}`,
    `engine config files        ${counts.engineConfigFiles.length}`,
    `release targets            ${counts.releaseTargets.length} (${counts.releaseTargets.join(", ")})`,
    `repository                 ${counts.repositorySlug}`,
  ];
  for (const [id, sites] of counts.duplicateFunctionIds) {
    lines.push(`  ! duplicate id ${id}: ${sites.map((s) => `${s.file}:${s.line}`).join(", ")}`);
  }
  for (const [route, sites] of counts.duplicateHttpRoutes) {
    lines.push(`  ! duplicate route ${route}: ${sites.map((s) => `${s.file}:${s.line}`).join(", ")}`);
  }
  return lines.join("\n");
}

function main(argv: string[]): number {
  const counts = collectCounts();

  if (argv.includes("--json")) {
    console.log(
      JSON.stringify(
        {
          workers: counts.workerCount,
          rustWorkers: counts.rustWorkerCount,
          pythonWorkers: counts.pythonWorkerCount,
          functionRegistrations: counts.functionRegistrationCount,
          functionIds: counts.functionIdCount,
          duplicateFunctionIds: [...counts.duplicateFunctionIds.keys()],
          duplicateHttpRoutes: [...counts.duplicateHttpRoutes.keys()],
          rustTestAttributes: counts.rustTestAttributes,
          ignoredRustTests: counts.ignoredRustTests,
          providers: counts.providers,
          cliSubcommands: counts.cliSubcommands.length,
          tuiScreens: counts.tuiScreens.length,
          ciJobs: counts.ciJobs.length,
          engineWorkers: counts.engineWorkers.length,
          engineConfigFiles: counts.engineConfigFiles.length,
          releaseTargets: counts.releaseTargets,
          repository: counts.repositorySlug,
        },
        null,
        2,
      ),
    );
    return 0;
  }

  if (argv.includes("--write")) {
    const changed = applyDrift(counts);
    for (const line of changed) console.log(`updated ${line}`);
    const remaining = findDrift(counts);
    for (const item of remaining) {
      console.error(`unwritable drift ${item.file}: ${item.label} (found ${item.found}, expected ${item.expected})`);
    }
    return remaining.length === 0 ? 0 : 1;
  }

  if (argv.includes("--check")) {
    const drift = findDrift(counts);
    if (drift.length === 0) {
      console.log(report(counts));
      console.log("\ncounts: every published number matches the tree");
      return 0;
    }
    console.error(report(counts));
    console.error("");
    for (const item of drift) {
      const where = item.line > 0 ? `${item.file}:${item.line}` : item.file;
      console.error(`drift ${where}: ${item.label}: published ${item.found}, tree says ${item.expected}`);
    }
    console.error(`\ncounts: ${drift.length} published value(s) disagree with the tree.`);
    console.error("Run `bun run counts:write` for the numeric ones; worker tables are edited by hand.");
    return 1;
  }

  console.log(report(counts));
  return 0;
}

if (import.meta.main) {
  process.exit(main(process.argv.slice(2)));
}
