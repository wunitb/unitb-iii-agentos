import { describe, expect, it } from "bun:test";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { withoutComments } from "../scripts/counts";

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

/**
 * Comments are stripped before matching, offsets preserved.
 *
 * `crates/http-adapter/src/state.rs` documents the three wrong shapes as
 * counterexamples, which is exactly the documentation that stops the mistake
 * recurring. A scanner that reads it reports the cure as the disease, and the
 * cheapest way to get green is then to delete the documentation — the failure
 * mode this whole remediation exists to fight.
 */
const sources = rustSources().map((file) => ({
  file,
  text: withoutComments(readFileSync(join(repositoryRoot, file), "utf8")),
}));

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

describe("state:: scanner", () => {
  const wrongUpdate = 'let payload = json!({ "scope": "s", "key": "k", "operations": [] });';
  const wrongIncrement = 'let op = json!({ "type": "increment", "path": "n", "value": 1 });';
  const wrongMerge = 'let op = json!({ "type": "merge", "path": "m", "value": [1] });';
  const withUpdateId = (line: string) => `let id = "state::update".to_string();\n${line}\n`;

  function scan(text: string): { operations: boolean; increment: boolean; merge: boolean } {
    const stripped = withoutComments(text);
    return {
      operations: /"state::update"[\s\S]{0,900}?"operations"\s*:/.test(stripped),
      increment: [...stripped.matchAll(/"type"\s*:\s*"increment"/g)].some(
        (match) => !/"by"\s*:/.test(enclosingObject(stripped, match.index)),
      ),
      merge: [...stripped.matchAll(/"type"\s*:\s*"merge"/g)].some((match) =>
        /"value"\s*:\s*\[/.test(enclosingObject(stripped, match.index)),
      ),
    };
  }

  it("does not flag a documented counterexample inside a comment", () => {
    const documented = [
      "//! The engine's `state::*` wire shapes.",
      "//!",
      '//! $ iii trigger "state::update" --json \'{"scope":"t","operations":[...]}\'',
      "//! Error: serialization error: missing field `ops`",
      '//! { "type": "increment", "path": "n", "value": 1 }   # wrong: it is `by`',
      '//! { "type": "merge", "path": "m", "value": [1] }      # wrong: merge takes an object',
      "",
      '/* Block form, and /* nested */ too:',
      '   json!({ "state::update": 1, "operations": [] })',
      "*/",
      'let good = json!({ "scope": "s", "key": "k", "ops": [{ "type": "increment", "path": "n", "by": 1 }] });',
    ].join("\n");

    expect(scan(documented)).toEqual({ operations: false, increment: false, merge: false });
  });

  it("flags the same text outside a comment", () => {
    expect(scan(withUpdateId(wrongUpdate)).operations).toBe(true);
    expect(scan(wrongIncrement).increment).toBe(true);
    expect(scan(wrongMerge).merge).toBe(true);
  });

  it("does not mistake a URL in a string for a comment", () => {
    // Blanking from the `//` of a URL to end of line would hide real code and
    // turn this scanner into a source of false negatives.
    const source = `let url = "ws://localhost:49134";\n${withUpdateId(wrongUpdate)}`;
    expect(withoutComments(source)).toContain('"ws://localhost:49134"');
    expect(scan(source).operations).toBe(true);
  });

  it("keeps line numbers stable while blanking", () => {
    const source = '// wrong: "operations"\nlet a = 1;\n/* two\n   lines */\nlet b = 2;\n';
    const stripped = withoutComments(source);
    expect(stripped.length).toBe(source.length);
    expect(stripped.split("\n").length).toBe(source.split("\n").length);
    expect(stripped).toContain("let a = 1;");
    expect(stripped).toContain("let b = 2;");
    expect(stripped).not.toContain("operations");
  });

  it("leaves a raw string that contains comment markers alone", () => {
    const source = 'let s = r#"/* not a comment */ // neither"#;\n';
    expect(withoutComments(source)).toBe(source);
  });
});
