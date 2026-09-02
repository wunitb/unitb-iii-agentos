import { describe, expect, it } from "bun:test";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

/**
 * Three `state::*` protocol mistakes were repo-wide, silent, and shipped for
 * months. Verified against the pinned engine (iii 0.22.1) on 2026-09-02:
 *
 *   1. `state::update` takes `ops`, not `operations`. Sending `operations`
 *      fails the whole invocation with "serialization error: missing field `ops`".
 *   2. An `increment` op carries `by`, not `value`. Same hard failure.
 *   3. `merge` requires an OBJECT value. Given an array it returns HTTP 200 with
 *      an `errors` array — a silent no-op. Appending to a list is
 *      `{"type":"append","path":X,"value":<ELEMENT>}`.
 *
 * This scanner is the cheap permanent guard. It is deliberately not
 * allowlisted: a site that still reads `operations` at merge time is the signal.
 */

const repositoryRoot = new URL("../", import.meta.url).pathname;
const SOURCE_ROOTS = ["workers", "crates"];
const SKIP_DIRECTORIES = new Set([".git", ".worktrees", "node_modules", "target"]);

function rustSources(): string[] {
  const found: string[] = [];
  const walk = (relative: string): void => {
    for (const entry of readdirSync(join(repositoryRoot, relative))) {
      if (SKIP_DIRECTORIES.has(entry)) continue;
      const path = join(relative, entry);
      if (statSync(join(repositoryRoot, path)).isDirectory()) walk(path);
      else if (entry.endsWith(".rs")) found.push(path);
    }
  };
  for (const root of SOURCE_ROOTS) walk(root);
  return found.sort();
}

function lineOf(source: string, index: number): number {
  return source.slice(0, index).split("\n").length;
}

/** The `{ ... }` literal that encloses `index`, brace-balanced in both directions. */
function enclosingObject(source: string, index: number): string {
  let depth = 0;
  let start = index;
  while (start >= 0) {
    const character = source[start];
    if (character === "}") depth += 1;
    else if (character === "{") {
      if (depth === 0) break;
      depth -= 1;
    }
    start -= 1;
  }
  if (start < 0) return "";
  depth = 0;
  let end = start;
  while (end < source.length) {
    const character = source[end];
    if (character === "{") depth += 1;
    else if (character === "}") {
      depth -= 1;
      if (depth === 0) break;
    }
    end += 1;
  }
  return source.slice(start, end + 1);
}

const sources = rustSources().map((file) => ({ file, text: readFileSync(join(repositoryRoot, file), "utf8") }));

/** How far after a `"state::update"` literal its `json!` payload can reasonably reach. */
const PAYLOAD_WINDOW = 900;

function updatePayloadsUsingOperations(): string[] {
  const offenders = new Set<string>();
  for (const { file, text } of sources) {
    for (const match of text.matchAll(/"state::update"/g)) {
      const window = text.slice(match.index, match.index + PAYLOAD_WINDOW);
      const key = /"operations"\s*:/.exec(window);
      if (key) offenders.add(`${file}:${lineOf(text, match.index + key.index)} state::update payload uses "operations"; the engine field is "ops"`);
    }
  }
  return [...offenders].sort();
}

function incrementOpsCarryingValue(): string[] {
  const offenders = new Set<string>();
  for (const { file, text } of sources) {
    for (const match of text.matchAll(/"type"\s*:\s*"increment"/g)) {
      const object = enclosingObject(text, match.index);
      if (/"by"\s*:/.test(object)) continue;
      offenders.add(`${file}:${lineOf(text, match.index)} increment op carries "value"; the engine field is "by"`);
    }
  }
  return [...offenders].sort();
}

function mergeOpsWithArrayValue(): string[] {
  const offenders = new Set<string>();
  for (const { file, text } of sources) {
    for (const match of text.matchAll(/"type"\s*:\s*"merge"/g)) {
      const object = enclosingObject(text, match.index);
      if (!/"value"\s*:\s*\[/.test(object)) continue;
      offenders.add(`${file}:${lineOf(text, match.index)} merge op has an array value; merge takes an object, appending takes "append"`);
    }
  }
  return [...offenders].sort();
}

describe("state::update wire protocol", () => {
  it("names the operation list \"ops\", never \"operations\"", () => {
    expect(updatePayloadsUsingOperations()).toEqual([]);
  });

  it("gives every increment op a \"by\" amount", () => {
    expect(incrementOpsCarryingValue()).toEqual([]);
  });

  it("never hands a merge op an array value", () => {
    expect(mergeOpsWithArrayValue()).toEqual([]);
  });

  it("scans the whole workspace, so an empty result means clean and not unscanned", () => {
    expect(sources.length).toBeGreaterThan(100);
    const withUpdate = sources.filter(({ text }) => text.includes('"state::update"'));
    expect(withUpdate.length, "no state::update call site found — the scanner is looking in the wrong place").toBeGreaterThan(0);
  });

  it("recognises the correct shapes as clean", () => {
    const good = 'json!({ "scope": "s", "key": "k", "ops": [{ "type": "increment", "path": "n", "by": 1 }, { "type": "append", "path": "xs", "value": 1 }, { "type": "merge", "path": "o", "value": { "a": 1 } }] })';
    const probe = [{ file: "probe.rs", text: `"state::update".to_string(), payload: ${good}` }];
    const scan = (finder: (batch: typeof probe) => string[]): string[] => finder(probe);
    expect(scan((batch) => batch.flatMap(({ text }) => (/"operations"\s*:/.test(text) ? ["bad"] : [])))).toEqual([]);
    expect(/"by"\s*:/.test(enclosingObject(good, good.indexOf('"increment"')))).toBe(true);
    expect(/"value"\s*:\s*\[/.test(enclosingObject(good, good.indexOf('"merge"')))).toBe(false);
  });
});
