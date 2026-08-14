import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { FleetStore, type FleetConfig, type FleetRequest } from "./fleet-core";

const roots: string[] = [];
const config: FleetConfig = {
  version: 3,
  session: "test",
  workspaceLabel: "test",
  repo: "owner/repo",
  runtimeDir: "runtime",
  worktreeDir: "worktrees",
  maxTeams: 2,
  credentialProxy: {
    bind: "127.0.0.1:49137",
    upstreamUrl: "http://127.0.0.1:8765",
    upstreamTokenFile: "~/.omp/auth-broker.token",
  },
  main: { model: "main-model", credentialSlot: 0 },
  teams: [
    { id: "TEAM-01", model: "team-model-1", credentialSlot: 0 },
    { id: "TEAM-02", model: "team-model-2", credentialSlot: 1 },
  ],
  reviewer: {
    id: "Reviewer",
    routes: {
      "TEAM-01": { model: "review-model-1", credentialSlot: 0 },
      "TEAM-02": { model: "review-model-2", credentialSlot: 1 },
    },
  },
};

function setup() {
  const root = mkdtempSync(join(tmpdir(), "unitb-fleet-"));
  roots.push(root);
  const store = new FleetStore(join(root, "fleet.sqlite"), root);
  const tokenPaths = store.bootstrap(config, "w1:p1");
  const tokens = Object.fromEntries(Object.entries(tokenPaths).map(([id, path]) => [id, readFileSync(path, "utf8").trim()]));
  return { root, store, tokens };
}

afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

function call(store: FleetStore, token: string, op: string, data: Record<string, unknown>, id: string = crypto.randomUUID()) {
  const request: FleetRequest = { id, op, token, data };
  return store.handle(request);
}

function contract(ownedPaths = ["src/feature"]) {
  return {
    goal: "Implement the assigned feature",
    ownedPaths,
    readOnlyPaths: ["package.json"],
    forbiddenPaths: [".github"],
    dependsOn: [],
    nonGoals: ["merge"],
    acceptance: ["Observable behavior works"],
    verification: ["bun test"],
    mergeAuthority: "principal",
  };
}

describe("FleetStore", () => {
  test("enforces the exact-head writer-reviewer-handoff lifecycle", () => {
    const { root, store, tokens } = setup();
    const base = "a".repeat(40);
    const head = "b".repeat(40);

    expect(call(store, tokens.Main, "plan", {
      workId: "WORK-01",
      principalGoal: "\"Build the feature\" — delegated implementation",
      repository: "owner/repo",
      verifiedBaseSha: base,
    }).ok).toBe(true);

    const assigned = call(store, tokens.Main, "assign", {
      workId: "WORK-01",
      teamId: "TEAM-01",
      worktree: join(root, "worktrees", "TEAM-01", "WORK-01"),
      contract: contract(),
    });
    expect(assigned.ok).toBe(true);
    const revision = (assigned.result as Record<string, string>).updated_at;

    expect(call(store, tokens["TEAM-01"], "ack", { workId: "WORK-01", contractRevision: "stale" }).ok).toBe(false);
    expect(call(store, tokens["TEAM-01"], "ack", { workId: "WORK-01", contractRevision: revision }).ok).toBe(true);
    expect(call(store, tokens["TEAM-01"], "submit", {
      workId: "WORK-01",
      exactHead: head,
      changedPaths: ["src/feature/index.ts"],
      verification: ["bun test: pass"],
    }).ok).toBe(true);

    expect(call(store, tokens.Reviewer, "review", {
      workId: "WORK-01",
      exactHead: base,
      verdict: "approved",
      findings: [],
    }).ok).toBe(false);
    expect(call(store, tokens.Reviewer, "review", {
      workId: "WORK-01",
      exactHead: head,
      verdict: "approved",
      findings: [],
    }).ok).toBe(true);

    expect(call(store, tokens["TEAM-01"], "handoff", {
      workId: "WORK-01",
      exactHead: head,
      remoteHead: head,
      pullRequest: "https://github.com/owner/repo/pull/1",
    }).ok).toBe(false);
    const handoff = call(store, tokens.Main, "handoff", {
      workId: "WORK-01",
      exactHead: head,
      remoteHead: head,
      pullRequest: "https://github.com/owner/repo/pull/1",
    });
    expect(handoff.ok).toBe(true);
    expect((handoff.result as Record<string, string>).state).toBe("handed_off");
    store.close();
  });

  test("serializes path ownership and command idempotency", () => {
    const { root, store, tokens } = setup();
    const base = "a".repeat(40);
    for (const workId of ["WORK-01", "WORK-02"]) {
      expect(call(store, tokens.Main, "plan", {
        workId,
        principalGoal: `\"${workId}\" — test`,
        repository: "owner/repo",
        verifiedBaseSha: base,
      }).ok).toBe(true);
    }

    const idempotencyKey = "assign-work-01";
    const first = call(store, tokens.Main, "assign", {
      workId: "WORK-01",
      teamId: "TEAM-01",
      worktree: join(root, "one"),
      contract: contract(["src/shared"]),
    }, idempotencyKey);
    const duplicate = call(store, tokens.Main, "assign", {
      workId: "WORK-01",
      teamId: "TEAM-01",
      worktree: join(root, "one"),
      contract: contract(["src/shared"]),
    }, idempotencyKey);
    expect(first).toEqual(duplicate);

    const conflict = call(store, tokens.Main, "assign", {
      workId: "WORK-02",
      teamId: "TEAM-02",
      worktree: join(root, "two"),
      contract: contract(["src/shared"]),
    });
    expect(conflict.ok).toBe(false);
    expect(conflict.error).toContain("locked by TEAM-01/WORK-01");
    expect(call(store, tokens["TEAM-01"], "plan", {
      workId: "WORK-03",
      principalGoal: "not allowed",
      repository: "owner/repo",
      verifiedBaseSha: base,
    }).ok).toBe(false);
    store.close();
  });

  test("persists authoritative state across dispatcher restart", () => {
    const { root, store, tokens } = setup();
    expect(call(store, tokens.Main, "plan", {
      workId: "WORK-01",
      principalGoal: "\"Persist\" — restart proof",
      repository: "owner/repo",
      verifiedBaseSha: "a".repeat(40),
    }).ok).toBe(true);
    store.close();

    const reopened = new FleetStore(join(root, "fleet.sqlite"), root);
    const status = call(reopened, tokens.Main, "status", { workId: "WORK-01" });
    expect(status.ok).toBe(true);
    expect(((status.result as { workItems: Array<Record<string, string>> }).workItems[0]).state).toBe("planned");
    reopened.close();
  });

  test("reapplies model assignments without rotating credentials", () => {
    const { store, tokens } = setup();
    const updatedConfig = structuredClone(config);
    updatedConfig.reviewer.routes["TEAM-01"].model = "replacement-review-model";

    const tokenPaths = store.bootstrap(updatedConfig);
    expect(readFileSync(tokenPaths.Reviewer, "utf8").trim()).toBe(tokens.Reviewer);

    const status = call(store, tokens.Main, "status", {});
    const reviewer = (status.result as { agents: Array<Record<string, string>> }).agents
      .find((agent) => agent.identity_id === "Reviewer");
    expect(reviewer?.model).toBe("TEAM-01:replacement-review-model|TEAM-02:review-model-2");
    store.close();
  });
});
