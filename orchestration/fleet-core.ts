import { Database } from "bun:sqlite";
import { mkdirSync, readFileSync, writeFileSync, chmodSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";

export const WORK_STATES = [
  "planned",
  "assigned",
  "implementing",
  "ready_for_review",
  "changes_requested",
  "handoff_ready",
  "handed_off",
  "merged",
  "blocked",
  "cancelled",
] as const;

export type WorkState = (typeof WORK_STATES)[number];
export type FleetRole = "main" | "team" | "reviewer";

export interface FleetModelConfig {
  model: string;
  credentialSlot: number;
}

export interface CredentialProxyConfig {
  bind: string;
  upstreamUrl: string;
  upstreamTokenFile: string;
}

export interface FleetConfig {
  version: number;
  session: string;
  workspaceLabel: string;
  repo: string;
  runtimeDir: string;
  worktreeDir: string;
  maxTeams: number;
  main: FleetModelConfig;
  teams: Array<{ id: string } & FleetModelConfig>;
  reviewer: {
    id: string;
    routes: Record<string, FleetModelConfig>;
  };
  credentialProxy: CredentialProxyConfig;
}

export interface AssignmentContract {
  goal: string;
  ownedPaths: string[];
  readOnlyPaths?: string[];
  forbiddenPaths?: string[];
  dependsOn?: string[];
  nonGoals?: string[];
  acceptance: string[];
  verification: string[];
  mergeAuthority: "principal";
}

export interface FleetRequest {
  id: string;
  op: string;
  token: string;
  data?: Record<string, unknown>;
}

export interface FleetResponse {
  ok: boolean;
  requestId: string;
  result?: unknown;
  error?: string;
}

export interface AuthIdentity {
  id: string;
  role: FleetRole;
  teamId?: string;
}

const TRANSITIONS: Record<WorkState, readonly WorkState[]> = {
  planned: ["assigned", "cancelled", "blocked"],
  assigned: ["implementing", "cancelled", "blocked"],
  implementing: ["ready_for_review", "cancelled", "blocked"],
  ready_for_review: ["changes_requested", "handoff_ready", "cancelled", "blocked"],
  changes_requested: ["implementing", "ready_for_review", "cancelled", "blocked"],
  handoff_ready: ["handed_off", "cancelled", "blocked"],
  handed_off: ["merged", "blocked"],
  merged: [],
  blocked: ["assigned", "implementing", "ready_for_review", "handoff_ready", "cancelled"],
  cancelled: [],
};

function now(): string {
  return new Date().toISOString();
}

function requiredString(value: unknown, field: string): string {
  if (typeof value !== "string" || value.trim() === "") {
    throw new Error(`${field} must be a non-empty string`);
  }
  return value.trim();
}

function stringArray(value: unknown, field: string, required = false): string[] {
  if (value === undefined && !required) return [];
  if (!Array.isArray(value) || value.some((entry) => typeof entry !== "string" || entry.trim() === "")) {
    throw new Error(`${field} must be an array of non-empty strings`);
  }
  if (required && value.length === 0) throw new Error(`${field} must not be empty`);
  return [...new Set(value.map((entry) => entry.trim()))];
}

function parseJson<T>(value: unknown, fallback: T): T {
  if (typeof value !== "string" || value === "") return fallback;
  return JSON.parse(value) as T;
}

function tokenHash(token: string): string {
  const hasher = new Bun.CryptoHasher("sha256");
  hasher.update(token);
  return hasher.digest("hex");
}

export function reviewerModelFor(config: FleetConfig, teamId: string): FleetModelConfig {
  const route = config.reviewer.routes[teamId];
  if (!route) throw new Error(`No reviewer model route configured for ${teamId}`);
  return route;
}

function reviewerModelPolicy(config: FleetConfig): string {
  return Object.entries(config.reviewer.routes)
    .sort(([left], [right]) => left.localeCompare(right))
    .map(([teamId, route]) => `${teamId}:${route.model}`)
    .join("|");
}

export function loadFleetConfig(path: string): FleetConfig {
  const parsed = JSON.parse(readFileSync(path, "utf8")) as FleetConfig;
  if (parsed.version !== 3) throw new Error(`Unsupported fleet config version: ${parsed.version}`);
  if (!Array.isArray(parsed.teams) || parsed.teams.length === 0 || parsed.teams.length > parsed.maxTeams) {
    throw new Error("teams must contain between 1 and maxTeams entries");
  }
  requiredString(parsed.main.model, "main.model");
  requiredString(parsed.reviewer.id, "reviewer.id");
  const ids = new Set(parsed.teams.map((team) => requiredString(team.id, "team.id")));
  if (ids.size !== parsed.teams.length || ids.has(parsed.reviewer.id) || ids.has("Main") || parsed.reviewer.id === "Main") {
    throw new Error("Fleet agent IDs must be unique");
  }
  const routeIds = Object.keys(parsed.reviewer.routes ?? {});
  if (routeIds.length !== ids.size || routeIds.some((teamId) => !ids.has(teamId))) {
    throw new Error("reviewer.routes must define exactly one route for every Team");
  }
  requiredString(parsed.credentialProxy?.bind, "credentialProxy.bind");
  requiredString(parsed.credentialProxy?.upstreamUrl, "credentialProxy.upstreamUrl");
  requiredString(parsed.credentialProxy?.upstreamTokenFile, "credentialProxy.upstreamTokenFile");
  for (const agent of [parsed.main, ...parsed.teams, ...Object.values(parsed.reviewer.routes)]) {
    requiredString(agent.model, "model");
    if (!Number.isSafeInteger(agent.credentialSlot) || agent.credentialSlot < 0) {
      throw new Error("credentialSlot must be a non-negative integer");
    }
  }
  return parsed;
}

export class FleetStore {
  readonly db: Database;
  readonly runtimeDir: string;

  constructor(dbPath: string, runtimeDir = dirname(dbPath)) {
    mkdirSync(dirname(dbPath), { recursive: true });
    this.runtimeDir = runtimeDir;
    this.db = new Database(dbPath, { create: true, strict: true });
    this.db.exec("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;");
    this.initialize();
  }

  close(): void {
    this.db.close();
  }

  private initialize(): void {
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS meta (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS auth_tokens (
        token_hash TEXT PRIMARY KEY,
        identity_id TEXT NOT NULL UNIQUE,
        role TEXT NOT NULL CHECK(role IN ('main','team','reviewer')),
        team_id TEXT
      );
      CREATE TABLE IF NOT EXISTS credential_assignments (
        identity_id TEXT PRIMARY KEY,
        provider TEXT NOT NULL,
        credential_slot INTEGER NOT NULL CHECK(credential_slot >= 0),
        FOREIGN KEY(identity_id) REFERENCES auth_tokens(identity_id)
      );
      CREATE TABLE IF NOT EXISTS work_items (
        id TEXT PRIMARY KEY,
        principal_goal TEXT NOT NULL,
        repository TEXT NOT NULL,
        verified_base_sha TEXT NOT NULL,
        team_id TEXT,
        state TEXT NOT NULL,
        state_before_block TEXT,
        status_reason TEXT,
        contract_json TEXT,
        active_model TEXT,
        active_session TEXT,
        worktree TEXT,
        branch TEXT,
        exact_head TEXT,
        remote_head TEXT,
        pull_request TEXT,
        review_verdict TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS commands (
        id TEXT PRIMARY KEY,
        op TEXT NOT NULL,
        actor_id TEXT NOT NULL,
        work_id TEXT,
        payload_json TEXT NOT NULL,
        result_json TEXT,
        status TEXT NOT NULL CHECK(status IN ('accepted','applied','failed')),
        error TEXT,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS events (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        at TEXT NOT NULL,
        actor_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        work_id TEXT,
        team_id TEXT,
        payload_json TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS agents (
        identity_id TEXT PRIMARY KEY,
        role TEXT NOT NULL,
        team_id TEXT,
        model TEXT NOT NULL,
        workspace_id TEXT,
        pane_id TEXT,
        session_ref TEXT,
        status TEXT NOT NULL DEFAULT 'unknown',
        current_work_id TEXT,
        last_seen_at TEXT NOT NULL
      );
      CREATE TABLE IF NOT EXISTS path_locks (
        path TEXT PRIMARY KEY,
        work_id TEXT NOT NULL,
        team_id TEXT NOT NULL,
        acquired_at TEXT NOT NULL,
        FOREIGN KEY(work_id) REFERENCES work_items(id)
      );
      CREATE TABLE IF NOT EXISTS messages (
        id TEXT PRIMARY KEY,
        work_id TEXT,
        sender TEXT NOT NULL,
        recipient TEXT NOT NULL,
        body TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'queued',
        created_at TEXT NOT NULL,
        delivered_at TEXT
      );
      CREATE INDEX IF NOT EXISTS events_work_idx ON events(work_id, seq);
      CREATE INDEX IF NOT EXISTS messages_recipient_idx ON messages(recipient, status, created_at);
    `);
  }

  bootstrap(config: FleetConfig, mainPaneId?: string, coordinationIssue?: string): Record<string, string> {
    const tokenDir = join(this.runtimeDir, "tokens");
    mkdirSync(tokenDir, { recursive: true, mode: 0o700 });
    chmodSync(tokenDir, 0o700);

    const reviewerDefault = reviewerModelFor(config, [...config.teams].sort((left, right) => left.id.localeCompare(right.id))[0]!.id);
    const identities: Array<{
      id: string;
      role: FleetRole;
      teamId?: string;
      model: string;
      credentialModel: string;
      credentialSlot: number;
    }> = [
      { id: "Main", role: "main", model: config.main.model, credentialModel: config.main.model, credentialSlot: config.main.credentialSlot },
      ...config.teams.map((team) => ({
        id: team.id,
        role: "team" as const,
        teamId: team.id,
        model: team.model,
        credentialModel: team.model,
        credentialSlot: team.credentialSlot,
      })),
      {
        id: config.reviewer.id,
        role: "reviewer",
        teamId: config.reviewer.id,
        model: reviewerModelPolicy(config),
        credentialModel: reviewerDefault.model,
        credentialSlot: reviewerDefault.credentialSlot,
      },
    ];
    const tokens: Record<string, string> = {};
    const insertToken = this.db.query(`
      INSERT INTO auth_tokens(token_hash, identity_id, role, team_id)
      VALUES(?, ?, ?, ?)
      ON CONFLICT(identity_id) DO UPDATE SET token_hash=excluded.token_hash, role=excluded.role, team_id=excluded.team_id
    `);
    const insertAgent = this.db.query(`
      INSERT INTO agents(identity_id, role, team_id, model, pane_id, last_seen_at)
      VALUES(?, ?, ?, ?, ?, ?)
      ON CONFLICT(identity_id) DO UPDATE SET role=excluded.role, team_id=excluded.team_id,
        model=excluded.model, pane_id=COALESCE(excluded.pane_id, agents.pane_id), last_seen_at=excluded.last_seen_at
    `);

    const insertCredentialAssignment = this.db.query(`
      INSERT INTO credential_assignments(identity_id, provider, credential_slot)
      VALUES(?, ?, ?)
      ON CONFLICT(identity_id) DO UPDATE SET
        provider=excluded.provider,
        credential_slot=excluded.credential_slot
    `);
    this.db.transaction(() => {
      for (const identity of identities) {
        const tokenPath = join(tokenDir, `${identity.id}.token`);
        const token = existsSync(tokenPath) ? readFileSync(tokenPath, "utf8").trim() : crypto.randomUUID() + crypto.randomUUID();
        writeFileSync(tokenPath, `${token}\n`, { mode: 0o600 });
        chmodSync(tokenPath, 0o600);
        tokens[identity.id] = tokenPath;
        insertToken.run(tokenHash(token), identity.id, identity.role, identity.teamId ?? null);
        insertAgent.run(
          identity.id,
          identity.role,
          identity.teamId ?? null,
          identity.model,
          identity.id === "Main" ? mainPaneId ?? null : null,
          now(),
        );
        insertCredentialAssignment.run(identity.id, identity.credentialModel.split("/", 1)[0], identity.credentialSlot);
      }
      this.setMeta("schema_version", "2");
      this.setMeta("session", config.session);
      this.setMeta("repo", config.repo);
      if (coordinationIssue) this.setMeta("coordination_issue", coordinationIssue);
    })();

    return tokens;
  }

  setMeta(key: string, value: string): void {
    this.db.query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value").run(key, value);
  }

  getMeta(key: string): string | undefined {
    const row = this.db.query("SELECT value FROM meta WHERE key=?").get(key) as { value: string } | null;
    return row?.value;
  }

  bindAgent(identityId: string, workspaceId: string, paneId: string, sessionRef?: string): void {
    const result = this.db.query(`
      UPDATE agents SET workspace_id=?, pane_id=?, session_ref=?, status='idle', last_seen_at=? WHERE identity_id=?
    `).run(workspaceId, paneId, sessionRef ?? null, now(), identityId);
    if (result.changes !== 1) throw new Error(`Unknown fleet identity: ${identityId}`);
    this.event("Dispatcher", "agent.bound", undefined, identityId, { workspaceId, paneId, sessionRef });
  }

  releaseAgent(identityId: string): void {
    const result = this.db.query(`
      UPDATE agents
      SET workspace_id=NULL, pane_id=NULL, session_ref=NULL, status='unknown', current_work_id=NULL, last_seen_at=?
      WHERE identity_id=?
    `).run(now(), identityId);
    if (result.changes !== 1) throw new Error(`Unknown fleet identity: ${identityId}`);
    this.event("Dispatcher", "agent.released", undefined, identityId, {});
  }

  updateAgentStatus(identityId: string, status: string, currentWorkId?: string): void {
    this.db.query(`
      UPDATE agents SET status=?, current_work_id=COALESCE(?, current_work_id), last_seen_at=? WHERE identity_id=?
    `).run(status, currentWorkId ?? null, now(), identityId);
  }

  handle(request: FleetRequest): FleetResponse {
    if (!request || typeof request !== "object") throw new Error("Request must be an object");
    const requestId = requiredString(request.id, "id");
    const op = requiredString(request.op, "op");
    const token = requiredString(request.token, "token");
    const actor = this.authenticate(token);
    const payload = request.data ?? {};

    const existing = this.db.query("SELECT status,result_json,error FROM commands WHERE id=?").get(requestId) as
      | { status: string; result_json: string | null; error: string | null }
      | null;
    if (existing) {
      return existing.status === "applied"
        ? { ok: true, requestId, result: parseJson(existing.result_json, null) }
        : { ok: false, requestId, error: existing.error ?? "Command did not apply" };
    }

    const createdAt = now();
    this.db.query(`
      INSERT INTO commands(id,op,actor_id,work_id,payload_json,status,created_at,updated_at)
      VALUES(?,?,?,?,?,'accepted',?,?)
    `).run(requestId, op, actor.id, typeof payload.workId === "string" ? payload.workId : null, JSON.stringify(payload), createdAt, createdAt);

    try {
      const result = this.db.transaction(() => this.apply(actor, op, payload))();
      this.db.query("UPDATE commands SET status='applied', result_json=?, updated_at=? WHERE id=?")
        .run(JSON.stringify(result ?? null), now(), requestId);
      return { ok: true, requestId, result };
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      this.db.query("UPDATE commands SET status='failed', error=?, updated_at=? WHERE id=?").run(message, now(), requestId);
      return { ok: false, requestId, error: message };
    }
  }

  authenticate(token: string): AuthIdentity {
    const row = this.db.query("SELECT identity_id,role,team_id FROM auth_tokens WHERE token_hash=?").get(tokenHash(token)) as
      | { identity_id: string; role: FleetRole; team_id: string | null }
      | null;
    if (!row) throw new Error("Invalid fleet token");
    return { id: row.identity_id, role: row.role, teamId: row.team_id ?? undefined };
  }

  assignCredential(identityId: string, model: string, credentialSlot: number): void {
    const provider = requiredString(model, "model").split("/", 1)[0];
    if (!Number.isSafeInteger(credentialSlot) || credentialSlot < 0) {
      throw new Error("credentialSlot must be a non-negative integer");
    }
    const result = this.db.query(`
      UPDATE credential_assignments SET provider=?, credential_slot=? WHERE identity_id=?
    `).run(provider, credentialSlot, identityId);
    if (result.changes !== 1) throw new Error(`Unknown fleet identity: ${identityId}`);
  }

  credentialAssignment(token: string): AuthIdentity & { provider: string; credentialSlot: number } {
    const actor = this.authenticate(token);
    const row = this.db.query(`
      SELECT provider, credential_slot FROM credential_assignments WHERE identity_id=?
    `).get(actor.id) as { provider: string; credential_slot: number } | null;
    if (!row) throw new Error(`No credential assignment for ${actor.id}`);
    return { ...actor, provider: row.provider, credentialSlot: row.credential_slot };
  }

  private apply(actor: AuthIdentity, op: string, data: Record<string, unknown>): unknown {
    switch (op) {
      case "plan": return this.plan(actor, data);
      case "assign": return this.assign(actor, data);
      case "ack": return this.ack(actor, data);
      case "report": return this.report(actor, data);
      case "submit": return this.submit(actor, data);
      case "review": return this.review(actor, data);
      case "handoff": return this.handoff(actor, data);
      case "cancel": return this.cancel(actor, data);
      case "message": return this.message(actor, data);
      case "status": return this.status(actor, data);
      default: throw new Error(`Unsupported fleet operation: ${op}`);
    }
  }

  private requireRole(actor: AuthIdentity, ...roles: FleetRole[]): void {
    if (!roles.includes(actor.role)) throw new Error(`${actor.role} cannot perform this operation`);
  }

  private work(workId: string): Record<string, unknown> {
    const row = this.db.query("SELECT * FROM work_items WHERE id=?").get(workId) as Record<string, unknown> | null;
    if (!row) throw new Error(`Unknown work item: ${workId}`);
    return row;
  }

  private assertAssigned(actor: AuthIdentity, work: Record<string, unknown>): void {
    if (actor.role === "main") return;
    if (work.team_id !== actor.teamId) throw new Error(`${actor.id} does not own work item ${work.id}`);
  }

  private transition(workId: string, next: WorkState, actor: AuthIdentity, reason?: string): void {
    const work = this.work(workId);
    const current = work.state as WorkState;
    if (!TRANSITIONS[current].includes(next)) throw new Error(`Invalid transition: ${current} -> ${next}`);
    let beforeBlock = work.state_before_block as WorkState | null;
    if (next === "blocked") beforeBlock = current;
    else if (next === "cancelled") beforeBlock = null;
    this.db.query(`
      UPDATE work_items SET state=?, state_before_block=?, status_reason=?, updated_at=? WHERE id=?
    `).run(next, beforeBlock, reason ?? null, now(), workId);
    if (["cancelled", "merged"].includes(next)) this.releaseLocks(workId);
    this.event(actor.id, `work.${next}`, workId, work.team_id as string | undefined, { from: current, reason });
  }

  private plan(actor: AuthIdentity, data: Record<string, unknown>): unknown {
    this.requireRole(actor, "main");
    const id = requiredString(data.workId, "workId");
    const goal = requiredString(data.principalGoal, "principalGoal");
    const repository = requiredString(data.repository, "repository");
    const base = requiredString(data.verifiedBaseSha, "verifiedBaseSha");
    if (!/^[0-9a-f]{40}$/.test(base)) throw new Error("verifiedBaseSha must be a full 40-character SHA");
    const timestamp = now();
    this.db.query(`
      INSERT INTO work_items(id,principal_goal,repository,verified_base_sha,state,created_at,updated_at)
      VALUES(?,?,?,?,'planned',?,?)
    `).run(id, goal, repository, base, timestamp, timestamp);
    this.event(actor.id, "work.planned", id, undefined, { goal, repository, verifiedBaseSha: base });
    return this.work(id);
  }

  private assign(actor: AuthIdentity, data: Record<string, unknown>): unknown {
    this.requireRole(actor, "main");
    const workId = requiredString(data.workId, "workId");
    const teamId = requiredString(data.teamId, "teamId");
    const work = this.work(workId);
    if (work.state !== "planned") throw new Error(`Work item ${workId} is not planned`);
    const team = this.db.query("SELECT identity_id,model FROM agents WHERE identity_id=? AND role='team'").get(teamId) as
      | { identity_id: string; model: string }
      | null;
    if (!team) throw new Error(`Unknown Team: ${teamId}`);

    const rawContract = data.contract as Record<string, unknown> | undefined;
    if (!rawContract) throw new Error("contract is required");
    const contract: AssignmentContract = {
      goal: requiredString(rawContract.goal, "contract.goal"),
      ownedPaths: stringArray(rawContract.ownedPaths, "contract.ownedPaths", true),
      readOnlyPaths: stringArray(rawContract.readOnlyPaths, "contract.readOnlyPaths"),
      forbiddenPaths: stringArray(rawContract.forbiddenPaths, "contract.forbiddenPaths"),
      dependsOn: stringArray(rawContract.dependsOn, "contract.dependsOn"),
      nonGoals: stringArray(rawContract.nonGoals, "contract.nonGoals"),
      acceptance: stringArray(rawContract.acceptance, "contract.acceptance", true),
      verification: stringArray(rawContract.verification, "contract.verification", true),
      mergeAuthority: "principal",
    };
    if (rawContract.mergeAuthority !== "principal") throw new Error("mergeAuthority must be principal");

    for (const dependency of contract.dependsOn ?? []) {
      const dependencyWork = this.work(dependency);
      if (!["handoff_ready", "handed_off", "merged"].includes(String(dependencyWork.state))) {
        throw new Error(`Dependency ${dependency} is not ready`);
      }
    }
    for (const path of contract.ownedPaths) {
      const conflict = this.db.query("SELECT work_id,team_id FROM path_locks WHERE path=?").get(path) as
        | { work_id: string; team_id: string }
        | null;
      if (conflict) throw new Error(`Path ${path} is locked by ${conflict.team_id}/${conflict.work_id}`);
    }

    const branch = `fleet/${teamId.toLowerCase()}/${workId.toLowerCase()}`;
    const worktree = requiredString(data.worktree, "worktree");
    this.db.query(`
      UPDATE work_items SET team_id=?, contract_json=?, active_model=?, worktree=?, branch=?, updated_at=? WHERE id=?
    `).run(teamId, JSON.stringify(contract), team.model, worktree, branch, now(), workId);
    for (const path of contract.ownedPaths) {
      this.db.query("INSERT INTO path_locks(path,work_id,team_id,acquired_at) VALUES(?,?,?,?)")
        .run(path, workId, teamId, now());
    }
    this.transition(workId, "assigned", actor, `Assigned to ${teamId}`);
    this.db.query("UPDATE agents SET current_work_id=?, status='assigned', last_seen_at=? WHERE identity_id=?")
      .run(workId, now(), teamId);
    return this.work(workId);
  }

  private ack(actor: AuthIdentity, data: Record<string, unknown>): unknown {
    this.requireRole(actor, "team");
    const workId = requiredString(data.workId, "workId");
    const work = this.work(workId);
    this.assertAssigned(actor, work);
    const revision = requiredString(data.contractRevision, "contractRevision");
    if (revision !== String(work.updated_at)) throw new Error("Stale assignment contract revision");
    this.transition(workId, "implementing", actor, "Contract acknowledged");
    this.updateAgentStatus(actor.id, "working", workId);
    return this.work(workId);
  }

  private report(actor: AuthIdentity, data: Record<string, unknown>): unknown {
    this.requireRole(actor, "team", "reviewer");
    const workId = requiredString(data.workId, "workId");
    const work = this.work(workId);
    if (actor.role === "team") this.assertAssigned(actor, work);
    const status = requiredString(data.status, "status");
    const reason = typeof data.reason === "string" ? data.reason : undefined;
    if (status === "blocked") {
      this.transition(workId, "blocked", actor, requiredString(reason, "reason"));
    } else if (status === "implementing" && work.state === "changes_requested") {
      this.transition(workId, "implementing", actor, reason ?? "Revision started");
    } else {
      this.event(actor.id, "work.progress", workId, work.team_id as string | undefined, {
        status,
        reason,
        evidence: data.evidence ?? null,
      });
    }
    this.updateAgentStatus(actor.id, status, workId);
    return this.work(workId);
  }

  private submit(actor: AuthIdentity, data: Record<string, unknown>): unknown {
    this.requireRole(actor, "team");
    const workId = requiredString(data.workId, "workId");
    const work = this.work(workId);
    this.assertAssigned(actor, work);
    if (!["implementing", "changes_requested"].includes(String(work.state))) {
      throw new Error(`Cannot submit work from state ${work.state}`);
    }
    const exactHead = requiredString(data.exactHead, "exactHead");
    if (!/^[0-9a-f]{40}$/.test(exactHead)) throw new Error("exactHead must be a full 40-character SHA");
    const changedPaths = stringArray(data.changedPaths, "changedPaths", true);
    const verification = stringArray(data.verification, "verification", true);
    this.validateOwnedPaths(work, changedPaths);
    this.db.query("UPDATE work_items SET exact_head=?, updated_at=? WHERE id=?").run(exactHead, now(), workId);
    this.transition(workId, "ready_for_review", actor, "Exact-head handoff submitted");
    this.event(actor.id, "handoff.submitted", workId, actor.teamId, { exactHead, changedPaths, verification });
    this.updateAgentStatus(actor.id, "idle", workId);
    return this.work(workId);
  }

  private review(actor: AuthIdentity, data: Record<string, unknown>): unknown {
    this.requireRole(actor, "reviewer");
    const workId = requiredString(data.workId, "workId");
    const work = this.work(workId);
    if (work.state !== "ready_for_review") throw new Error(`Work item ${workId} is not ready for review`);
    const exactHead = requiredString(data.exactHead, "exactHead");
    if (exactHead !== work.exact_head) throw new Error("Reviewer exactHead does not match submitted head");
    const verdict = requiredString(data.verdict, "verdict");
    if (verdict !== "approved" && verdict !== "changes_requested") {
      throw new Error("verdict must be approved or changes_requested");
    }
    const findings = stringArray(data.findings, "findings");
    this.db.query("UPDATE work_items SET review_verdict=?, updated_at=? WHERE id=?").run(verdict, now(), workId);
    this.transition(workId, verdict === "approved" ? "handoff_ready" : "changes_requested", actor, findings.join("; ") || verdict);
    this.event(actor.id, "review.verdict", workId, work.team_id as string | undefined, { exactHead, verdict, findings });
    return this.work(workId);
  }

  private handoff(actor: AuthIdentity, data: Record<string, unknown>): unknown {
    this.requireRole(actor, "main");
    const workId = requiredString(data.workId, "workId");
    const work = this.work(workId);
    if (work.state !== "handoff_ready" || work.review_verdict !== "approved") {
      throw new Error("Only approved handoff-ready work can be handed to the Principal");
    }
    const exactHead = requiredString(data.exactHead, "exactHead");
    const remoteHead = requiredString(data.remoteHead, "remoteHead");
    if (exactHead !== work.exact_head || remoteHead !== exactHead) throw new Error("Local and remote exact heads must match reviewed head");
    const pullRequest = requiredString(data.pullRequest, "pullRequest");
    this.db.query("UPDATE work_items SET remote_head=?, pull_request=?, updated_at=? WHERE id=?")
      .run(remoteHead, pullRequest, now(), workId);
    this.transition(workId, "handed_off", actor, "Principal merge handoff recorded");
    this.releaseLocks(workId);
    return this.work(workId);
  }

  private cancel(actor: AuthIdentity, data: Record<string, unknown>): unknown {
    this.requireRole(actor, "main");
    const workId = requiredString(data.workId, "workId");
    const reason = requiredString(data.reason, "reason");
    const work = this.work(workId);
    if (["merged", "cancelled"].includes(String(work.state))) throw new Error(`Work item ${workId} is terminal`);
    this.transition(workId, "cancelled", actor, reason);
    if (typeof work.team_id === "string") this.updateAgentStatus(work.team_id, "cancelled", workId);
    return this.work(workId);
  }

  private message(actor: AuthIdentity, data: Record<string, unknown>): unknown {
    const recipient = requiredString(data.recipient, "recipient");
    const body = requiredString(data.body, "body");
    const workId = typeof data.workId === "string" ? data.workId : undefined;
    if (workId) {
      const work = this.work(workId);
      if (actor.role !== "main" && work.team_id !== actor.teamId && actor.role !== "reviewer") {
        throw new Error(`${actor.id} cannot message about work item ${workId}`);
      }
    }
    const known = this.db.query("SELECT identity_id FROM agents WHERE identity_id=?").get(recipient);
    if (!known) throw new Error(`Unknown recipient: ${recipient}`);
    const id = requiredString(data.messageId, "messageId");
    this.db.query("INSERT INTO messages(id,work_id,sender,recipient,body,created_at) VALUES(?,?,?,?,?,?)")
      .run(id, workId ?? null, actor.id, recipient, body, now());
    this.event(actor.id, "message.queued", workId, actor.teamId, { messageId: id, recipient });
    return { id, sender: actor.id, recipient, workId, status: "queued" };
  }

  private status(actor: AuthIdentity, data: Record<string, unknown>): unknown {
    const workId = typeof data.workId === "string" ? data.workId : undefined;
    const works = workId
      ? [this.work(workId)]
      : (this.db.query("SELECT * FROM work_items ORDER BY created_at DESC").all() as Array<Record<string, unknown>>);
    const visible = actor.role === "main" || actor.role === "reviewer"
      ? works
      : works.filter((work) => work.team_id === actor.teamId);
    const agents = this.db.query("SELECT * FROM agents ORDER BY identity_id").all();
    const queuedMessages = this.db.query("SELECT * FROM messages WHERE recipient=? AND status='queued' ORDER BY created_at")
      .all(actor.id);
    return { actor: actor.id, workItems: visible, agents, messages: queuedMessages };
  }

  private validateOwnedPaths(work: Record<string, unknown>, changedPaths: string[]): void {
    const contract = parseJson<AssignmentContract>(work.contract_json, {} as AssignmentContract);
    const owned = contract.ownedPaths ?? [];
    const forbidden = contract.forbiddenPaths ?? [];
    for (const changedPath of changedPaths) {
      if (forbidden.some((path) => changedPath === path || changedPath.startsWith(`${path}/`))) {
        throw new Error(`Changed forbidden path: ${changedPath}`);
      }
      if (!owned.some((path) => changedPath === path || changedPath.startsWith(`${path}/`))) {
        throw new Error(`Changed path is outside assignment ownership: ${changedPath}`);
      }
    }
  }

  private releaseLocks(workId: string): void {
    this.db.query("DELETE FROM path_locks WHERE work_id=?").run(workId);
  }

  private event(actorId: string, kind: string, workId?: string, teamId?: string, payload: unknown = {}): void {
    this.db.query("INSERT INTO events(at,actor_id,kind,work_id,team_id,payload_json) VALUES(?,?,?,?,?,?)")
      .run(now(), actorId, kind, workId ?? null, teamId ?? null, JSON.stringify(payload));
  }
}
