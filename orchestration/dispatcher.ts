#!/usr/bin/env bun
import { chmodSync, copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer, createConnection } from "node:net";
import { dirname, join, resolve } from "node:path";
import { FleetStore, loadFleetConfig, reviewerModelFor, type FleetConfig, type FleetRequest, type FleetResponse } from "./fleet-core";
import { startCredentialProxy } from "./credential-proxy";

const ROOT = resolve(import.meta.dir, "..");
const DEFAULT_CONFIG = join(import.meta.dir, "fleet.config.json");
const HOST_OMP_AGENT_DIR = resolve(process.env.HOME ?? "", ".omp/agent");
const HOST_HERDR_EXTENSION = join(HOST_OMP_AGENT_DIR, "extensions", "herdr-omp-agent-state.ts");
const FLEET_EXTENSION = join(import.meta.dir, "fleet-extension.ts");
const ROLE_GUARD = join(import.meta.dir, "role-guard.ts");
const OMP_WRAPPER_DIR = join(import.meta.dir, "bin");

interface RuntimePaths {
  runtimeDir: string;
  worktreeDir: string;
  socket: string;
  database: string;
}

function pathsFor(config: FleetConfig): RuntimePaths {
  const runtimeDir = resolve(ROOT, config.runtimeDir);
  return {
    runtimeDir,
    worktreeDir: resolve(ROOT, config.worktreeDir),
    socket: join(runtimeDir, "dispatcher.sock"),
    database: join(runtimeDir, "fleet.sqlite"),
  };
}

function parseArgs(argv: string[]): { command: string; configPath: string; rest: string[] } {
  const args = [...argv];
  let configPath = DEFAULT_CONFIG;
  const configIndex = args.indexOf("--config");
  if (configIndex >= 0) {
    const value = args[configIndex + 1];
    if (!value) throw new Error("--config requires a path");
    configPath = resolve(value);
    args.splice(configIndex, 2);
  }
  return { command: args.shift() ?? "help", configPath, rest: args };
}

async function spawnChecked(command: string[], cwd = ROOT): Promise<string> {
  const proc = Bun.spawn(command, { cwd, stdout: "pipe", stderr: "pipe", env: process.env });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  if (exitCode !== 0) {
    throw new Error(`${command[0]} exited ${exitCode}: ${(stderr || stdout).trim()}`);
  }
  return stdout.trim();
}

function findString(value: unknown, key: string): string | undefined {
  if (!value || typeof value !== "object") return undefined;
  if (key in value && typeof (value as Record<string, unknown>)[key] === "string") {
    return (value as Record<string, string>)[key];
  }
  for (const child of Object.values(value as Record<string, unknown>)) {
    const found = findString(child, key);
    if (found) return found;
  }
  return undefined;
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", `'"'"'`)}'`;
}

async function stageOmpAgent(
  agentRuntime: string,
  model: string,
): Promise<{
  agentDir: string;
  config: string;
  extension: string;
  fleetExtension: string;
  roleGuard: string;
  home: string;
}> {
  const agentDir = join(agentRuntime, "omp-agent");
  const home = join(agentRuntime, "home");
  const config = join(agentDir, "config.yml");
  const extensionDir = join(agentDir, "extensions");
  const extension = join(extensionDir, "herdr-omp-agent-state.ts");
  const fleetExtension = join(extensionDir, "fleet-extension.ts");
  const roleGuard = join(extensionDir, "role-guard.ts");
  rmSync(join(agentRuntime, "model-key-name"), { force: true });
  rmSync(join(agentRuntime, "model-key-value"), { force: true });

  rmSync(agentDir, { recursive: true, force: true });
  mkdirSync(extensionDir, { recursive: true, mode: 0o700 });
  rmSync(home, { recursive: true, force: true });
  mkdirSync(home, { recursive: true, mode: 0o700 });
  copyFileSync(HOST_HERDR_EXTENSION, extension);
  copyFileSync(FLEET_EXTENSION, fleetExtension);
  copyFileSync(ROLE_GUARD, roleGuard);
  const configuredModel = JSON.stringify(model);
  writeFileSync(config, [
    "setupVersion: 1",
    "modelRoles:",
    `  plan: ${configuredModel}`,
    `  slow: ${configuredModel}`,
    `  designer: ${configuredModel}`,
    `  advisor: ${configuredModel}`,
    `  task: ${configuredModel}`,
    `  smol: ${configuredModel}`,
    `  tiny: ${configuredModel}`,
    `  commit: ${configuredModel}`,
    `  vision: ${configuredModel}`,
    `  default: ${configuredModel}`,
    "tools:",
    "  approvalMode: yolo",
    "task:",
    "  maxConcurrency: 1",
    "  maxRecursionDepth: 0",
    "compaction:",
    "  thresholdTokens: 150000",
    "",
  ].join("\n"), { mode: 0o600 });
  const changelogMarker = join(HOST_OMP_AGENT_DIR, "last-changelog-version");
  if (existsSync(changelogMarker)) {
    copyFileSync(changelogMarker, join(agentDir, "last-changelog-version"));
  }
  return { agentDir, config, extension, fleetExtension, roleGuard, home };
}

function unitName(prefix: string, identity: string): string {
  return `${prefix}-${identity.toLowerCase().replaceAll(/[^a-z0-9-]/g, "-")}.service`;
}

async function stopUnit(name: string): Promise<void> {
  const proc = Bun.spawn(["systemctl", "--user", "stop", name], { stdout: "ignore", stderr: "ignore" });
  await proc.exited;
  const reset = Bun.spawn(["systemctl", "--user", "reset-failed", name], { stdout: "ignore", stderr: "ignore" });
  await reset.exited;
}

async function startProxy(config: FleetConfig, runtime: RuntimePaths, identity: string, paneId: string): Promise<string> {
  const agentRuntime = join(runtime.runtimeDir, "agents", identity);
  mkdirSync(agentRuntime, { recursive: true, mode: 0o700 });
  const listenPath = join(agentRuntime, "herdr.sock");
  if (existsSync(listenPath)) rmSync(listenPath);
  const upstream = resolve(process.env.HOME ?? "", `.config/herdr/sessions/${config.session}/herdr.sock`);
  const unit = unitName("unitb-herdr-proxy", identity);
  await stopUnit(unit);
  await spawnChecked([
    "systemd-run",
    "--user",
    `--unit=${unit}`,
    "--property=Type=simple",
    "--property=Restart=on-failure",
    "--property=RestartSec=1",
    `--working-directory=${ROOT}`,
    process.execPath,
    join(import.meta.dir, "herdr-proxy.ts"),
    "--listen",
    listenPath,
    "--upstream",
    upstream,
    "--pane",
    paneId,
  ]);

  for (let attempt = 0; attempt < 50; attempt += 1) {
    if (existsSync(listenPath)) return listenPath;
    await Bun.sleep(50);
  }
  throw new Error(`Herdr proxy did not create ${listenPath}`);
}

function promptFor(identity: string, role: "team" | "reviewer", work: Record<string, unknown>): string {
  const contract = JSON.parse(String(work.contract_json)) as Record<string, unknown>;
  if (role === "reviewer") {
    return [
      `You are ${identity}, the independent read-only reviewer for work ${work.id}.`,
      `Review exact commit ${work.exact_head}; never modify the source branch or merge.`,
      "Run focused verification in this disposable review worktree. Return a structured verdict through fleet_review.",
      `Contract: ${JSON.stringify(contract)}`,
    ].join("\n");
  }
  return [
    `You are ${identity}, the sole writer for work ${work.id}.`,
    `Principal goal: ${work.principal_goal}`,
    `Base SHA: ${work.verified_base_sha}`,
    `Branch: ${work.branch}`,
    "Acknowledge the exact contract revision with fleet_ack before editing.",
    "Do not spawn subagents, merge, push, edit paths outside ownedPaths, or communicate outside fleet tools.",
    "Commit the complete change, verify it, then call fleet_submit with exact HEAD, changed paths, and commands actually run.",
    `Contract revision: ${work.updated_at}`,
    `Contract: ${JSON.stringify(contract)}`,
  ].join("\n");
}

async function createTeamWorktree(config: FleetConfig, runtime: RuntimePaths, work: Record<string, unknown>): Promise<{ workspaceId: string; paneId: string; worktree: string }> {
  const teamId = String(work.team_id);
  const worktree = String(work.worktree);
  mkdirSync(dirname(worktree), { recursive: true });
  const output = await spawnChecked([
    "herdr",
    "--session",
    config.session,
    "worktree",
    "create",
    "--workspace",
    await workspaceId(config),
    "--branch",
    String(work.branch),
    "--base",
    String(work.verified_base_sha),
    "--path",
    worktree,
    "--label",
    teamId,
    "--no-focus",
  ]);
  const parsed = JSON.parse(output) as unknown;
  const paneId = findString(parsed, "pane_id");
  const workspace = findString(parsed, "workspace_id");
  if (!paneId || !workspace) throw new Error(`Herdr worktree response omitted pane/workspace IDs: ${output}`);
  return { workspaceId: workspace, paneId, worktree };
}

async function createReviewWorktree(config: FleetConfig, runtime: RuntimePaths, work: Record<string, unknown>): Promise<{ workspaceId: string; paneId: string; worktree: string }> {
  const reviewRoot = join(runtime.worktreeDir, "reviews");
  mkdirSync(reviewRoot, { recursive: true });
  const worktree = join(reviewRoot, `${work.id}-${String(work.exact_head).slice(0, 12)}`);
  const output = await spawnChecked([
    "herdr",
    "--session",
    config.session,
    "worktree",
    "create",
    "--workspace",
    await workspaceId(config),
    "--base",
    String(work.exact_head),
    "--path",
    worktree,
    "--label",
    config.reviewer.id,
    "--no-focus",
  ]);
  const parsed = JSON.parse(output) as unknown;
  const paneId = findString(parsed, "pane_id");
  const workspace = findString(parsed, "workspace_id");
  if (!paneId || !workspace) throw new Error(`Herdr review worktree response omitted pane/workspace IDs: ${output}`);
  return { workspaceId: workspace, paneId, worktree };
}

async function workspaceIds(config: FleetConfig): Promise<Set<string>> {
  const output = await spawnChecked(["herdr", "--session", config.session, "workspace", "list"]);
  const parsed = JSON.parse(output) as { result?: { workspaces?: Array<{ workspace_id?: string }> } };
  return new Set(parsed.result?.workspaces?.flatMap((workspace) => workspace.workspace_id ? [workspace.workspace_id] : []) ?? []);
}

async function hasWorkspace(config: FleetConfig, expectedId: string): Promise<boolean> {
  return (await workspaceIds(config)).has(expectedId);
}

async function removePlacement(
  config: FleetConfig,
  identity: string,
  placement: { workspaceId: string },
): Promise<void> {
  const failures: string[] = [];
  try {
    await stopUnit(unitName("unitb-herdr-proxy", identity));
  } catch (error) {
    failures.push(`proxy: ${error instanceof Error ? error.message : String(error)}`);
  }
  try {
    if (await hasWorkspace(config, placement.workspaceId)) {
      await spawnChecked([
        "herdr",
        "--session",
        config.session,
        "worktree",
        "remove",
        "--workspace",
        placement.workspaceId,
        "--force",
      ]);
    }
  } catch (error) {
    failures.push(`worktree: ${error instanceof Error ? error.message : String(error)}`);
  }
  if (failures.length > 0) throw new Error(failures.join("; "));
}

async function reconcilePlacements(config: FleetConfig, store: FleetStore): Promise<void> {
  const existing = await workspaceIds(config);
  const agents = store.db.query("SELECT identity_id, workspace_id FROM agents WHERE workspace_id IS NOT NULL")
    .all() as Array<{ identity_id: string; workspace_id: string }>;
  for (const agent of agents) {
    if (existing.has(agent.workspace_id)) continue;
    await stopUnit(unitName("unitb-herdr-proxy", agent.identity_id));
    store.releaseAgent(agent.identity_id);
  }
}


async function workspaceId(config: FleetConfig): Promise<string> {
  const output = await spawnChecked(["herdr", "--session", config.session, "workspace", "list"]);
  const parsed = JSON.parse(output) as { result?: { workspaces?: Array<{ workspace_id?: string; label?: string }> } };
  const workspace = parsed.result?.workspaces?.find((candidate) => candidate.label === config.workspaceLabel);
  if (!workspace?.workspace_id) throw new Error(`Herdr workspace not found: ${config.workspaceLabel}`);
  return workspace.workspace_id;
}

async function configurePane(
  config: FleetConfig,
  runtime: RuntimePaths,
  identity: string,
  role: "team" | "reviewer",
  paneId: string,
  worktree: string,
  model: string,
  prompt: string,
): Promise<void> {
  const agentRuntime = join(runtime.runtimeDir, "agents", identity);
  const tokenSource = join(runtime.runtimeDir, "tokens", `${identity}.token`);
  const tokenTarget = join(agentRuntime, "fleet.token");
  const agentName = identity.toLowerCase();
  const ompState = await stageOmpAgent(agentRuntime, model);
  const agentConfigPath = join(agentRuntime, "fleet.config.json");
  copyFileSync(DEFAULT_CONFIG, agentConfigPath);
  const promptPath = join(agentRuntime, "assignment-prompt.md");
  writeFileSync(promptPath, prompt, { mode: 0o600 });
  copyFileSync(tokenSource, tokenTarget);
  chmodSync(tokenTarget, 0o600);
  const lifecycleSocket = await startProxy(config, runtime, identity, paneId);
  const gitDir = (await spawnChecked(["git", "rev-parse", "--git-dir"], worktree)).trim();
  const gitWorktreeDir = resolve(worktree, gitDir);
  const gitCommon = resolve(ROOT, ".git");
  const branchNamespace = join(gitCommon, "refs", "heads", "fleet", identity.toLowerCase());
  const logNamespace = join(gitCommon, "logs", "refs", "heads", "fleet", identity.toLowerCase());
  mkdirSync(branchNamespace, { recursive: true });
  mkdirSync(logNamespace, { recursive: true });
  mkdirSync(join(agentRuntime, "sessions"), { recursive: true });
  const gitAuthorName = await spawnChecked(["git", "config", "--global", "user.name"]);
  const gitAuthorEmail = await spawnChecked(["git", "config", "--global", "user.email"]);
  writeFileSync(join(agentRuntime, "dispatcher.sock"), "", { mode: 0o600 });

  const environment: Record<string, string> = {
    PATH: `${OMP_WRAPPER_DIR}:${process.env.PATH ?? ""}`,
    UNITB_FLEET_ROLE: role,
    UNITB_FLEET_ID: identity,
    UNITB_FLEET_CONFIG: agentConfigPath,
    UNITB_FLEET_RUNTIME_ROOT: runtime.runtimeDir,
    UNITB_FLEET_AGENT_RUNTIME: agentRuntime,
    UNITB_FLEET_WORKTREE: worktree,
    UNITB_FLEET_REPO_ROOT: ROOT,
    UNITB_FLEET_TOKEN_FILE: tokenTarget,
    UNITB_REAL_OMP: resolve(process.env.HOME ?? "", ".bun/bin/omp"),
    UNITB_GIT_COMMON: gitCommon,
    UNITB_GIT_WORKTREE_DIR: gitWorktreeDir,
    UNITB_FLEET_SOCKET: join(agentRuntime, "dispatcher.sock"),
    UNITB_FLEET_SOCKET_HOST: runtime.socket,
    UNITB_GIT_BRANCH_NAMESPACE: branchNamespace,
    UNITB_GIT_LOG_NAMESPACE: logNamespace,
    HERDR_SOCKET_PATH: lifecycleSocket,
    SSH_AUTH_SOCK: "",
    UNITB_FLEET_AGENT_HOME: ompState.home,
    OMP_AUTH_BROKER_URL: `http://${config.credentialProxy.bind}`,
    PI_CODING_AGENT_DIR: ompState.agentDir,
    GIT_AUTHOR_NAME: gitAuthorName,
    GIT_AUTHOR_EMAIL: gitAuthorEmail,
    GIT_COMMITTER_NAME: gitAuthorName,
    GIT_COMMITTER_EMAIL: gitAuthorEmail,
    HERDR_ENV: "1",
    HERDR_PANE_ID: paneId,
  };
  const exportCommand = `export ${Object.entries(environment).map(([key, value]) => `${key}=${shellQuote(value)}`).join(" ")}`;
  await spawnChecked(["herdr", "--session", config.session, "pane", "run", paneId, exportCommand]);

  const args = [
    "herdr",
    "--session",
    config.session,
    "agent",
    "start",
    agentName,
    "--kind",
    "omp",
    "--pane",
    paneId,
    "--timeout",
    "120000",
    "--",
    `--model=${model}`,
    "--auto-approve",
    "--no-extensions",
    "-e",
    ompState.extension,
    "-e",
    ompState.fleetExtension,
    "-e",
    ompState.roleGuard,
    "--config",
    ompState.config,
    "--session-dir",
    join(agentRuntime, "sessions"),
    "--no-prewalk",
    `@${promptPath}`,
  ];
  await spawnChecked(args, worktree);
}

async function launchAgent(config: FleetConfig, runtime: RuntimePaths, store: FleetStore, workId: string, role: "team" | "reviewer"): Promise<unknown> {
  const work = store.db.query("SELECT * FROM work_items WHERE id=?").get(workId) as Record<string, unknown> | null;
  if (!work) throw new Error(`Unknown work item: ${workId}`);
  if (role === "team" && work.state !== "assigned") throw new Error(`Team launch requires assigned state, got ${work.state}`);
  if (role === "reviewer" && work.state !== "ready_for_review") throw new Error(`Reviewer launch requires ready_for_review state, got ${work.state}`);

  const identity = role === "team" ? String(work.team_id) : config.reviewer.id;
  const agentConfig = role === "team"
    ? config.teams.find((team) => team.id === identity)
    : reviewerModelFor(config, String(work.team_id));
  if (!agentConfig) throw new Error(`No model configured for ${identity}`);
  const prompt = promptFor(identity, role, work);

  const previous = store.db.query("SELECT workspace_id, status FROM agents WHERE identity_id=?")
    .get(identity) as { workspace_id: string | null; status: string } | null;
  if (previous?.workspace_id) {
    if (previous.status === "working" || previous.status === "blocked") {
      throw new Error(`${identity} already has an active ${previous.status} session`);
    }
    await removePlacement(config, identity, { workspaceId: previous.workspace_id });
    store.releaseAgent(identity);
  }

  const placement = role === "team"
    ? await createTeamWorktree(config, runtime, work)
    : await createReviewWorktree(config, runtime, work);
  store.assignCredential(identity, agentConfig.model, agentConfig.credentialSlot);
  try {
    await configurePane(
      config,
      runtime,
      identity,
      role,
      placement.paneId,
      placement.worktree,
      agentConfig.model,
      prompt,
    );
    store.bindAgent(identity, placement.workspaceId, placement.paneId);
    store.updateAgentStatus(identity, "idle", workId);
    return { identity, role, model: agentConfig.model, ...placement };
  } catch (error) {
    try {
      await removePlacement(config, identity, placement);
    } catch (cleanupError) {
      const launchMessage = error instanceof Error ? error.message : String(error);
      const cleanupMessage = cleanupError instanceof Error ? cleanupError.message : String(cleanupError);
      throw new Error(`${launchMessage}; cleanup failed: ${cleanupMessage}`, { cause: error });
    }
    throw error;
  }
}

async function cancelAgent(config: FleetConfig, store: FleetStore, workId: string): Promise<void> {
  const work = store.db.query("SELECT team_id FROM work_items WHERE id=?").get(workId) as { team_id: string | null } | null;
  if (!work?.team_id) return;
  const proc = Bun.spawn(["herdr", "--session", config.session, "agent", "send-keys", work.team_id.toLowerCase(), "CTRL_C"], {
    stdout: "ignore",
    stderr: "ignore",
  });
  await proc.exited;
}

async function mirrorEvent(store: FleetStore, request: FleetRequest, response: FleetResponse): Promise<void> {
  if (!response.ok || ["status"].includes(request.op)) return;
  const issue = store.getMeta("coordination_issue");
  if (!issue) return;
  const match = issue.match(/^https:\/\/github\.com\/([^/]+\/[^/]+)\/issues\/(\d+)$/);
  if (!match) return;
  const workId = typeof request.data?.workId === "string" ? request.data.workId : "fleet";
  const body = [
    `<!-- unitb-fleet-event:${request.id} -->`,
    `**${request.op}** · ${workId}`,
    "```json",
    JSON.stringify(response.result ?? {}, null, 2).slice(0, 5000),
    "```",
  ].join("\n");
  const proc = Bun.spawn(["gh", "issue", "comment", match[2], "--repo", match[1], "--body", body], {
    cwd: ROOT,
    stdout: "ignore",
    stderr: "pipe",
  });
  const stderr = await new Response(proc.stderr).text();
  if ((await proc.exited) !== 0) {
    store.db.query("INSERT INTO events(at,actor_id,kind,payload_json) VALUES(?,?,?,?)")
      .run(new Date().toISOString(), "Dispatcher", "mirror.failed", JSON.stringify({ requestId: request.id, error: stderr.trim() }));
  }
}

async function processRequest(config: FleetConfig, runtime: RuntimePaths, store: FleetStore, request: FleetRequest): Promise<FleetResponse> {
  if (request.op === "launch_team" || request.op === "launch_reviewer") {
    const auth = store.handle({ ...request, op: "status", id: `${request.id}:auth`, data: { workId: request.data?.workId } });
    if (!auth.ok || (auth.result as { actor?: string } | undefined)?.actor !== "Main") {
      return { ok: false, requestId: request.id, error: "Only Main can launch fleet agents" };
    }
    try {
      const workId = String(request.data?.workId ?? "");
      const result = await launchAgent(config, runtime, store, workId, request.op === "launch_team" ? "team" : "reviewer");
      const response = { ok: true, requestId: request.id, result };
      await mirrorEvent(store, request, response);
      return response;
    } catch (error) {
      return { ok: false, requestId: request.id, error: error instanceof Error ? error.message : String(error) };
    }
  }

  const response = store.handle(request);
  if (response.ok && request.op === "cancel" && typeof request.data?.workId === "string") {
    await cancelAgent(config, store, request.data.workId);
  }
  await mirrorEvent(store, request, response);
  return response;
}

async function serve(config: FleetConfig, runtime: RuntimePaths): Promise<void> {
  mkdirSync(runtime.runtimeDir, { recursive: true, mode: 0o700 });
  if (existsSync(runtime.socket)) rmSync(runtime.socket);
  const store = new FleetStore(runtime.database, runtime.runtimeDir);
  store.bootstrap(config);
  await reconcilePlacements(config, store);
  const credentialProxy = startCredentialProxy(config.credentialProxy, (token) => store.credentialAssignment(token));
  const server = createServer((socket) => {
    socket.setEncoding("utf8");
    let buffer = "";
    socket.on("data", (chunk) => {
      buffer += chunk;
      const newline = buffer.indexOf("\n");
      if (newline < 0) {
        if (buffer.length > 1_000_000) socket.destroy(new Error("Request too large"));
        return;
      }
      const line = buffer.slice(0, newline);
      buffer = "";
      void (async () => {
        try {
          const request = JSON.parse(line) as FleetRequest;
          const response = await processRequest(config, runtime, store, request);
          socket.end(`${JSON.stringify(response)}\n`);
        } catch (error) {
          socket.end(`${JSON.stringify({ ok: false, requestId: "unknown", error: error instanceof Error ? error.message : String(error) })}\n`);
        }
      })();
    });
  });
  server.listen(runtime.socket, () => chmodSync(runtime.socket, 0o600));
  const shutdown = () => {
    server.close();
    credentialProxy.stop(true);
    store.close();
    if (existsSync(runtime.socket)) rmSync(runtime.socket);
    process.exit(0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
  const { promise } = Promise.withResolvers<void>();
  await promise;
}

async function sendRequest(socketPath: string, request: FleetRequest): Promise<FleetResponse> {
  const { promise, resolve: resolveResponse, reject } = Promise.withResolvers<FleetResponse>();
  const socket = createConnection(socketPath);
  let buffer = "";
  socket.setEncoding("utf8");
  socket.on("connect", () => socket.write(`${JSON.stringify(request)}\n`));
  socket.on("data", (chunk) => {
    buffer += chunk;
    const newline = buffer.indexOf("\n");
    if (newline >= 0) {
      socket.end();
      resolveResponse(JSON.parse(buffer.slice(0, newline)) as FleetResponse);
    }
  });
  socket.on("error", reject);
  return promise;
}

async function main(): Promise<void> {
  const { command, configPath, rest } = parseArgs(Bun.argv.slice(2));
  const config = loadFleetConfig(configPath);
  const runtime = pathsFor(config);
  if (command === "serve") {
    await serve(config, runtime);
    return;
  }
  if (command === "bootstrap") {
    const mainPaneIndex = rest.indexOf("--main-pane");
    const issueIndex = rest.indexOf("--coordination-issue");
    const store = new FleetStore(runtime.database, runtime.runtimeDir);
    const tokens = store.bootstrap(
      config,
      mainPaneIndex >= 0 ? rest[mainPaneIndex + 1] : undefined,
      issueIndex >= 0 ? rest[issueIndex + 1] : undefined,
    );
    store.close();
    console.log(JSON.stringify({ ok: true, runtime, tokens }));
    return;
  }
  if (command === "request") {
    const raw = rest[0] ?? readFileSync(0, "utf8");
    const response = await sendRequest(runtime.socket, JSON.parse(raw) as FleetRequest);
    console.log(JSON.stringify(response));
    if (!response.ok) process.exitCode = 1;
    return;
  }
  if (command === "health") {
    const store = new FleetStore(runtime.database, runtime.runtimeDir);
    const schema = store.getMeta("schema_version");
    store.close();
    console.log(JSON.stringify({ ok: schema === "2" && existsSync(runtime.socket), socket: existsSync(runtime.socket), schema }));
    return;
  }
  console.error("Usage: dispatcher.ts [--config path] serve|bootstrap|request|health");
  process.exitCode = 2;
}

await main();
