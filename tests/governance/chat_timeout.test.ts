import { describe, expect, it } from "bun:test";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

/**
 * iii-sdk 0.22.1 gives a trigger only 30 seconds when `timeout_ms` is absent.
 * A chat trigger owns the whole ReAct turn, including tool calls, so every hop
 * must select a bounded turn budget instead of silently inheriting that SDK
 * default. The HTTP adapter is also a hop: fixing only the inner workers still
 * lets the edge abandon a live and billable turn after 30 seconds.
 */

const repositoryRoot = fileURLToPath(new URL("../../", import.meta.url));

function trackedRustFiles(): string[] {
  const listed = Bun.spawnSync({
    cmd: ["git", "ls-files", "-z", "--", "*.rs"],
    cwd: repositoryRoot,
    stdout: "pipe",
    stderr: "pipe",
  });
  expect(listed.exitCode, listed.stderr.toString()).toBe(0);
  return listed.stdout.toString().split("\0").filter((path) => path.length > 0);
}

function read(path: string): string {
  return readFileSync(`${repositoryRoot}${path}`, "utf8");
}

/** Return balanced `TriggerRequest { ... }` expressions from Rust source. */
function triggerRequests(source: string): string[] {
  const requests: string[] = [];
  const starts = /\bTriggerRequest\s*\{/g;
  while (true) {
    const match = starts.exec(source);
    if (!match) return requests;
    const start = match.index;
    const brace = source.indexOf("{", start);

    let depth = 0;
    let end = brace;
    for (; end < source.length; end += 1) {
      if (source[end] === "{") depth += 1;
      if (source[end] === "}") depth -= 1;
      if (depth === 0) {
        end += 1;
        break;
      }
    }
    if (depth !== 0) return requests;
    requests.push(source.slice(start, end));
    starts.lastIndex = end;
  }
}

function firstLine(block: string): string {
  return block.split("\n").slice(0, 8).join(" ").replaceAll(/\s+/g, " ").slice(0, 180);
}

describe("chat calls keep the full bounded turn budget", () => {
  const rustFiles = trackedRustFiles();
  const chatCalls: Array<{ path: string; block: string }> = [];
  for (const path of rustFiles) {
    for (const block of triggerRequests(read(path))) {
      if (/function_id:\s*"agent::chat"\.to_string\(\)/.test(block)) {
        chatCalls.push({ path, block });
      }
    }
  }

  it("leaves no literal agent chat trigger on the SDK default", () => {
    // Hand runner and swarm derive a smaller deadline from an explicit outer
    // budget. Every other direct caller must name the shared chat ceiling; an
    // arbitrary `Some(30_000)` would be explicit but would recreate the outage.
    const dynamicBudgetCallers = new Set([
      "workers/hand-runner/src/main.rs",
      "workers/swarm/src/main.rs",
    ]);
    const offenders = chatCalls
      .filter(({ path, block }) => {
        if (/timeout_ms:\s*Some\s*\(CHAT_TIMEOUT_MS\)/.test(block)) return false;
        return !(
          dynamicBudgetCallers.has(path) &&
          /timeout_ms:\s*Some\s*\(timeout_ms\)/.test(block)
        );
      })
      .map(({ path, block }) => `${path}: ${firstLine(block)}`);

    expect(
      offenders,
      "a literal agent::chat TriggerRequest can inherit iii-sdk's 30-second default; " +
        "pass CHAT_TIMEOUT_MS or one of the reviewed bounded dynamic budgets",
    ).toEqual([]);
  });

  it("bounds the hand runner's per-iteration budget and its missing-limit fallback", () => {
    const handRunner = read("workers/hand-runner/src/main.rs");
    expect(handRunner).toMatch(/\.map_or\(CHAT_TIMEOUT_MS, \|n\| n\.saturating_mul\(5000\)\)/);
    expect(handRunner).toMatch(/\.min\(CHAT_TIMEOUT_MS\)/);
  });

  it("gives the HTTP adapter forward an explicit bounded timeout", () => {
    const path = "crates/http-adapter/src/lib.rs";
    const forwards = triggerRequests(read(path)).filter((block) =>
      block.includes("payload: normalize_http_request(request)"),
    );
    expect(forwards.length, "the adapter forward moved; update this guard with the implementation").toBe(1);
    expect(forwards[0]).toMatch(/timeout_ms:\s*Some\s*\(CHAT_TIMEOUT_MS\)/);
  });

  it("locks polling-only channel budgets to the canonical chat ceiling", () => {
    // These workers intentionally have no inbound HTTP adapter. Pulling that
    // crate in only for a constant would violate the boundary guarded by each
    // worker's `registers_no_inbound_http_route` regression. Their local
    // constants are therefore allowed only while they equal the canonical
    // adapter value and while this list names the whole exception surface.
    const canonical = read("crates/http-adapter/src/bus.rs").match(
      /pub const CHAT_TIMEOUT_MS: u64 = ([\d_]+);/,
    );
    expect(canonical, "the canonical chat ceiling moved; update this guard deliberately").not.toBeNull();

    const pollingOnly = [
      "workers/channel-bluesky/src/main.rs",
      "workers/channel-email/src/main.rs",
      "workers/channel-mastodon/src/main.rs",
      "workers/channel-reddit/src/main.rs",
      "workers/channel-signal/src/main.rs",
    ];
    const localBudgetCallers = [...new Set(
      chatCalls
        .map(({ path }) => path)
        .filter((path) => /^const CHAT_TIMEOUT_MS: u64 =/m.test(read(path))),
    )].sort();
    expect(localBudgetCallers).toEqual(pollingOnly);

    for (const path of pollingOnly) {
      const local = read(path).match(/^const CHAT_TIMEOUT_MS: u64 = ([\d_]+);/m);
      expect(local, `${path} must declare its bounded local chat ceiling`).not.toBeNull();
      expect(local![1].replaceAll("_", "")).toBe(canonical![1].replaceAll("_", ""));
      expect(read(path)).not.toContain("agentos_http_adapter");
    }
  });

  it("scans a meaningful tree and finds the known chat surface", () => {
    expect(rustFiles.length).toBeGreaterThan(100);
    expect(rustFiles).toContain("crates/http-adapter/src/lib.rs");
    expect(rustFiles).toContain("workers/channel-slack/src/main.rs");
    expect(rustFiles).toContain("workers/a2a/src/main.rs");
    expect(rustFiles).toContain("workers/hand-runner/src/main.rs");
    expect(rustFiles).toContain("workers/pulse/src/main.rs");
    expect(chatCalls.length).toBeGreaterThan(15);
  });
});
