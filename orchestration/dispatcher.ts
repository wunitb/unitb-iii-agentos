#!/usr/bin/env bun
import { chmodSync, copyFileSync, existsSync, mkdirSync, readFileSync, realpathSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { createServer, createConnection } from "node:net";
import { resolve4 } from "node:dns/promises";
import { dirname, join, resolve } from "node:path";
import { FLEET_SCHEMA_VERSION, FleetStore, loadFleetConfig, reviewerModelFor, type FleetConfig, type FleetRequest, type FleetResponse } from "./fleet-core";
import { startCredentialProxy } from "./credential-proxy";

const ROOT = resolve(import.meta.dir, "..");
const DEFAULT_CONFIG = join(import.meta.dir, "fleet.config.json");
const HOST_HOME = process.env.HOME ?? "";
const HOST_OMP_AGENT_DIR = resolve(HOST_HOME, ".omp/agent");
const HOST_HERDR_EXTENSION = join(HOST_OMP_AGENT_DIR, "extensions", "herdr-omp-agent-state.ts");
const FLEET_EXTENSION = join(import.meta.dir, "fleet-extension.ts");
const ROLE_GUARD = join(import.meta.dir, "role-guard.ts");
const OMP_WRAPPER_DIR = join(import.meta.dir, "bin");

interface RuntimePaths {
  runtimeDir: string;
  worktreeDir: string;
  socket: string;
  credentialSocket: string;
  database: string;
}

function pathsFor(config: FleetConfig): RuntimePaths {
  const runtimeDir = resolve(ROOT, config.runtimeDir);
  return {
    runtimeDir,
    worktreeDir: resolve(ROOT, config.worktreeDir),
    socket: join(runtimeDir, "dispatcher.sock"),
    credentialSocket: join(runtimeDir, "credential.sock"),
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

async function spawnCheckedRaw(command: string[], cwd = ROOT): Promise<string> {
  const proc = Bun.spawn(command, { cwd, stdout: "pipe", stderr: "pipe", env: process.env });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  if (exitCode !== 0) {
    throw new Error(`${command[0]} exited ${exitCode}: ${(stderr || stdout).trim()}`);
  }
  return stdout;
}

async function spawnChecked(command: string[], cwd = ROOT): Promise<string> {
  return (await spawnCheckedRaw(command, cwd)).trim();
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
async function stageNetworkPolicy(config: FleetConfig, model: string, agentRuntime: string): Promise<{
  hosts: string;
  resolv: string;
  nft: string;
}> {
  const provider = model.split("/", 1)[0];
  const allowedHosts = config.network.allowedHostsByProvider[provider] ?? [];
  if (allowedHosts.length === 0) throw new Error(`No network allowlist configured for model provider ${provider}`);
  const addresses = new Map<string, string[]>();
  for (const host of allowedHosts) {
    const resolved = [...new Set(await resolve4(host))].sort();
    if (resolved.length === 0) throw new Error(`No IPv4 address resolved for allowed host ${host}`);
    addresses.set(host, resolved);
  }
  const networkDir = join(agentRuntime, "network");
  rmSync(networkDir, { recursive: true, force: true });
  mkdirSync(networkDir, { recursive: true, mode: 0o700 });
  const hosts = join(networkDir, "hosts");
  const resolv = join(networkDir, "resolv.conf");
  const nft = join(networkDir, "egress.nft");
  writeFileSync(hosts, [
    "127.0.0.1 localhost",
    "::1 localhost",
    ...[...addresses].map(([host, ips]) => `${ips[0]} ${host}`),
    "",
  ].join("\n"), { mode: 0o400 });
  writeFileSync(resolv, `nameserver ${config.network.dnsForward}\noptions attempts:1 timeout:2\n`, { mode: 0o400 });
  const allowedIps = [...new Set([...addresses.values()].flat())].sort();
  writeFileSync(nft, [
    "flush ruleset",
    "table inet unitb_fleet {",
    "  chain input {",
    "    type filter hook input priority filter; policy drop;",
    "    iifname \"lo\" accept",
    "    ct state established,related accept",
    "  }",
    "  chain output {",
    "    type filter hook output priority filter; policy drop;",
    "    oifname \"lo\" accept",
    "    ct state established,related accept",
    ...allowedIps.map((ip) => `    ip daddr ${ip} tcp dport 443 accept`),
    "  }",
    "}",
    "",
  ].join("\n"), { mode: 0o400 });
  return { hosts, resolv, nft };
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
  toolchain: string;
  ompPackage: string;
  bun: string;
}> {
  const hostOmpEntry = realpathSync(resolve(HOST_HOME, ".bun/bin/omp"));
  const hostOmpPackage = resolve(dirname(hostOmpEntry), "..");
  const hostNodeModules = resolve(hostOmpPackage, "..", "..");
  const hostBun = realpathSync(process.execPath);
  const agentDir = join(agentRuntime, "omp-agent");
  const home = join(agentRuntime, "home");
  const toolchain = join(agentRuntime, "toolchain");
  const nodeModules = join(toolchain, "node_modules");
  const ompPackage = join(nodeModules, "@oh-my-pi/pi-coding-agent");
  const bun = join(toolchain, "bun");
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
  rmSync(toolchain, { recursive: true, force: true });
  mkdirSync(ompPackage, { recursive: true, mode: 0o700 });
  const packages = [
    [hostOmpPackage, ompPackage],
    [join(hostNodeModules, "@babel/parser"), join(nodeModules, "@babel/parser")],
    [join(hostNodeModules, "@oh-my-pi/pi-natives"), join(nodeModules, "@oh-my-pi/pi-natives")],
    [join(hostNodeModules, "@oh-my-pi/pi-natives-linux-arm64"), join(nodeModules, "@oh-my-pi/pi-natives-linux-arm64")],
  ];
  for (const [source, target] of packages) {
    mkdirSync(target, { recursive: true, mode: 0o700 });
    await spawnChecked(["cp", "-aL", "--reflink=auto", `${source}/.`, target]);
  }
  copyFileSync(hostBun, bun);
  chmodSync(bun, 0o500);
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
    "  approvalMode: write",
    "  approval:",
    "    bash: prompt",
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
  return { agentDir, config, extension, fleetExtension, roleGuard, home, toolchain, ompPackage, bun };
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
  const upstream = resolve(HOST_HOME, `.config/herdr/sessions/${config.session}/herdr.sock`);
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

function mainPrompt(config: FleetConfig): string {
  return [
    "You are Main, the fleet control-plane agent and the principal's interactive entry point.",
    `Repository: ${config.repo}`,
    `Available writer teams: ${config.teams.map((team) => team.id).join(", ")}`,
    `Independent reviewer: ${config.reviewer.id}`,
    "Translate each principal request into explicit work items and assignment contracts.",
    "Create plans without guessing Git state: fleet.plan resolves the active origin's current default-branch head.",
    "Use fleet tools to plan, assign, launch or relaunch, monitor, message, review, hand off, cancel, and finish work.",
    "Before handoff, independently inspect the exact Team commit and its verification evidence; never approve your own work or replace Reviewer.",
    "Never edit source, run mutating shell commands, push, merge, review, approve, or resolve findings.",
    "Ordinary completion ends at an exact-head pull-request handoff; the principal or a separately authorized PrincipalMergeAgent owns merge authority.",
  ].join("\n");
}

async function registerWorkspace(config: FleetConfig, cwd: string, label: string): Promise<{ workspaceId: string; paneId: string }> {
  const output = await spawnChecked([
    "herdr",
    "--session",
    config.session,
    "workspace",
    "create",
    "--cwd",
    cwd,
    "--label",
    label,
    "--no-focus",
  ]);
  const parsed = JSON.parse(output) as unknown;
  const paneId = findString(parsed, "pane_id");
  const workspaceId = findString(parsed, "workspace_id");
  if (!paneId || !workspaceId) throw new Error(`Herdr workspace response omitted pane/workspace IDs: ${output}`);
  return { paneId, workspaceId };
}

async function createTeamWorktree(config: FleetConfig, runtime: RuntimePaths, work: Record<string, unknown>): Promise<{ workspaceId: string; paneId: string; worktree: string }> {
  const teamId = String(work.team_id);
  const worktree = String(work.worktree);
  rmSync(worktree, { recursive: true, force: true });
  mkdirSync(dirname(worktree), { recursive: true });
  await spawnChecked(["git", "clone", "--quiet", "--no-local", "--no-hardlinks", ROOT, worktree]);
  await spawnChecked(["git", "checkout", "--detach", String(work.verified_base_sha)], worktree);
  await spawnChecked(["git", "switch", "-c", String(work.branch)], worktree);
  const placement = await registerWorkspace(config, worktree, teamId);
  return { ...placement, worktree };
}

function reviewWorktree(runtime: RuntimePaths, work: Record<string, unknown>): string {
  return join(runtime.worktreeDir, "reviews", `${work.id}-${String(work.exact_head).slice(0, 12)}`);
}

async function createReviewWorktree(config: FleetConfig, runtime: RuntimePaths, work: Record<string, unknown>): Promise<{ workspaceId: string; paneId: string; worktree: string }> {
  const worktree = reviewWorktree(runtime, work);
  mkdirSync(dirname(worktree), { recursive: true });
  rmSync(worktree, { recursive: true, force: true });
  await spawnChecked(["git", "clone", "--quiet", "--no-local", "--no-hardlinks", ROOT, worktree]);
  await spawnChecked(["git", "checkout", "--detach", String(work.exact_head)], worktree);
  const placement = await registerWorkspace(config, worktree, config.reviewer.id);
  return { ...placement, worktree };
}

async function workspaceIds(config: FleetConfig): Promise<Set<string>> {
  const output = await spawnChecked(["herdr", "--session", config.session, "workspace", "list"]);
  const parsed = JSON.parse(output) as { result?: { workspaces?: Array<{ workspace_id?: string }> } };
  return new Set(parsed.result?.workspaces?.flatMap((workspace) => workspace.workspace_id ? [workspace.workspace_id] : []) ?? []);
}

async function hasWorkspace(config: FleetConfig, expectedId: string): Promise<boolean> {
  return (await workspaceIds(config)).has(expectedId);
}
async function hasLiveAgent(config: FleetConfig, paneId: string): Promise<boolean> {
  const output = await spawnChecked(["herdr", "--session", config.session, "agent", "list"]);
  const parsed = JSON.parse(output) as {
    result?: { agents?: Array<{ agent?: string | null; agent_status?: string; pane_id?: string }> };
  };
  const agent = parsed.result?.agents?.find((candidate) => candidate.pane_id === paneId);
  return typeof agent?.agent === "string" && ["idle", "working", "blocked", "done"].includes(agent.agent_status ?? "");
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
        "workspace",
        "close",
        placement.workspaceId,
      ]);
    }
  } catch (error) {
    failures.push(`workspace: ${error instanceof Error ? error.message : String(error)}`);
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
  store: FleetStore,
  identity: string,
  role: "main" | "team" | "reviewer",
  paneId: string,
  worktree: string,
  model: string,
  prompt: string,
): Promise<void> {
  const agentRuntime = join(runtime.runtimeDir, "agents", identity);
  const tokenTarget = join(agentRuntime, "fleet.token");
  const agentName = identity.toLowerCase();
  const ompState = await stageOmpAgent(agentRuntime, model);
  const network = await stageNetworkPolicy(config, model, agentRuntime);
  const agentConfigPath = join(agentRuntime, "fleet.config.json");
  rmSync(agentConfigPath, { force: true });
  writeFileSync(agentConfigPath, `${JSON.stringify(config, null, 2)}\n`, { mode: 0o400 });
  const promptPath = join(agentRuntime, "assignment-prompt.md");
  rmSync(promptPath, { force: true });
  writeFileSync(promptPath, prompt, { mode: 0o400 });
  const lifecycleSocket = await startProxy(config, runtime, identity, paneId);
  mkdirSync(join(agentRuntime, "sessions"), { recursive: true, mode: 0o700 });
  const gitAuthorName = await spawnChecked(["git", "config", "--global", "user.name"]);
  const gitAuthorEmail = await spawnChecked(["git", "config", "--global", "user.email"]);
  rmSync(tokenTarget, { force: true });
  copyFileSync(store.rotateAgentToken(identity), tokenTarget);
  chmodSync(tokenTarget, 0o400);

  const environment: Record<string, string> = {
    PATH: `${OMP_WRAPPER_DIR}:${process.env.PATH ?? ""}`,
    UNITB_FLEET_ROLE: role,
    UNITB_FLEET_ID: identity,
    UNITB_FLEET_CONFIG: agentConfigPath,
    UNITB_FLEET_AGENT_RUNTIME: agentRuntime,
    UNITB_FLEET_WORKTREE: worktree,
    UNITB_FLEET_REPO_ROOT: ROOT,
    UNITB_FLEET_TOKEN_FILE: tokenTarget,
    UNITB_ASSIGNMENT_PROMPT: promptPath,
    UNITB_FLEET_SOCKET_HOST: runtime.socket,
    UNITB_CREDENTIAL_SOCKET_HOST: runtime.credentialSocket,
    UNITB_NETWORK_HOSTS: network.hosts,
    UNITB_NETWORK_RESOLV: network.resolv,
    UNITB_NETWORK_NFT: network.nft,
    UNITB_STAGED_OMP: ompState.ompPackage,
    UNITB_STAGED_BUN: ompState.bun,
    UNITB_HERDR_SOCKET_HOST: lifecycleSocket,
    SSH_AUTH_SOCK: "",
    UNITB_STAGED_TOOLCHAIN: ompState.toolchain,
    UNITB_FLEET_AGENT_HOME: ompState.home,
    OMP_AUTH_BROKER_URL: "http://127.0.0.1:8765",
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

  try {
    await spawnChecked([
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
      "--append-system-prompt",
      promptPath,
    ], worktree);
  } catch (error) {
    // Herdr may time out after OMP has registered; the live pane is authoritative.
    if (!await hasLiveAgent(config, paneId)) throw error;
  }
}

async function launchMain(config: FleetConfig, runtime: RuntimePaths, store: FleetStore): Promise<unknown> {
  const identity = "Main";
  const previous = store.agentPlacement(identity);
  if (typeof previous.workspace_id === "string") {
    const placementLive = typeof previous.pane_id === "string"
      && await hasWorkspace(config, previous.workspace_id)
      && await hasLiveAgent(config, previous.pane_id);
    if (placementLive) {
      return {
        identity,
        role: "main",
        model: config.main.model,
        workspaceId: previous.workspace_id,
        paneId: previous.pane_id,
        worktree: ROOT,
        existing: true,
      };
    }
    await removePlacement(config, identity, { workspaceId: previous.workspace_id });
    store.releaseAgent(identity);
  }

  const placement = await registerWorkspace(config, ROOT, config.workspaceLabel);
  store.assignCredential(identity, config.main.model, config.main.credentialId);
  store.bindAgent(identity, placement.workspaceId, placement.paneId);
  try {
    await configurePane(
      config,
      runtime,
      store,
      identity,
      "main",
      placement.paneId,
      ROOT,
      config.main.model,
      mainPrompt(config),
    );
    store.updateAgentStatus(identity, "idle");
    return { identity, role: "main", model: config.main.model, ...placement, worktree: ROOT, existing: false };
  } catch (error) {
    store.revokeAgentToken(identity);
    rmSync(join(runtime.runtimeDir, "agents", identity), { recursive: true, force: true });
    await spawnChecked(["herdr", "--session", config.session, "workspace", "close", placement.workspaceId])
      .catch(() => undefined);
    store.releaseAgent(identity);
    throw error;
  }
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
  const previous = store.db.query("SELECT workspace_id, pane_id, current_work_id, status FROM agents WHERE identity_id=?")
    .get(identity) as {
      workspace_id: string | null;
      pane_id: string | null;
      current_work_id: string | null;
      status: string;
    } | null;
  if (previous?.workspace_id) {
    const placementLive = typeof previous.pane_id === "string"
      && await hasWorkspace(config, previous.workspace_id)
      && await hasLiveAgent(config, previous.pane_id);
    if (placementLive && previous.current_work_id === workId) {
      return {
        identity,
        role,
        model: agentConfig.model,
        workspaceId: previous.workspace_id,
        paneId: previous.pane_id,
        worktree: role === "team" ? String(work.worktree) : reviewWorktree(runtime, work),
        existing: true,
      };
    }
    if (placementLive && (previous.status === "working" || previous.status === "blocked")) {
      throw new Error(`${identity} already has an active ${previous.status} session`);
    }
    await removePlacement(config, identity, { workspaceId: previous.workspace_id });
    store.releaseAgent(identity);
  }

  const placement = role === "team"
    ? await createTeamWorktree(config, runtime, work)
    : await createReviewWorktree(config, runtime, work);
  store.assignCredential(identity, agentConfig.model, agentConfig.credentialId);
  store.bindAgent(identity, placement.workspaceId, placement.paneId, workId);
  try {
    await configurePane(
      config,
      runtime,
      store,
      identity,
      role,
      placement.paneId,
      placement.worktree,
      agentConfig.model,
      prompt,
    );
    store.updateAgentStatus(identity, "idle", workId);
    return { identity, role, model: agentConfig.model, ...placement };
  } catch (error) {
    store.revokeAgentToken(identity);
    rmSync(join(runtime.runtimeDir, "agents", identity), { recursive: true, force: true });
    try {
      await removePlacement(config, identity, placement);
      store.releaseAgent(identity);
    } catch (cleanupError) {
      const launchMessage = error instanceof Error ? error.message : String(error);
      const cleanupMessage = cleanupError instanceof Error ? cleanupError.message : String(cleanupError);
      throw new Error(`${launchMessage}; cleanup failed: ${cleanupMessage}`, { cause: error });
    }
    throw error;
  }
}
function reviewRef(workId: string): string {
  const hasher = new Bun.CryptoHasher("sha256");
  hasher.update(workId);
  return `refs/heads/fleet/reviews/${hasher.digest("hex").slice(0, 24)}`;
}

function normalizedPaths(paths: unknown): string[] {
  if (!Array.isArray(paths) || paths.some((path) => typeof path !== "string")) {
    throw new Error("changedPaths must be an array of repository-relative paths");
  }
  const normalized = paths.map((path) => {
    const value = (path as string).replace(/\/+$/, "");
    if (
      value === ""
      || value.startsWith("/")
      || value.includes("\\")
      || value.includes("\0")
      || value.split("/").some((component) => component === "" || component === "." || component === "..")
    ) {
      throw new Error(`Invalid changed path: ${JSON.stringify(path)}`);
    }
    return value;
  });
  if (new Set(normalized).size !== normalized.length) throw new Error("changedPaths contains duplicates");
  return normalized.sort();
}

export async function changedPathsSince(worktree: string, base: string, exactHead: string): Promise<string[]> {
  return (await spawnCheckedRaw(["git", "diff", "--no-renames", "--name-only", "-z", base, exactHead], worktree))
    .split("\0")
    .filter(Boolean)
    .sort();
}

async function validateAndImportSubmission(work: Record<string, unknown>, data: Record<string, unknown>): Promise<void> {
  const worktree = String(work.worktree);
  const exactHead = String(data.exactHead ?? "");
  const branch = String(work.branch);
  const base = String(work.verified_base_sha);
  const currentHead = await spawnChecked(["git", "rev-parse", "HEAD"], worktree);
  const branchHead = await spawnChecked(["git", "rev-parse", `refs/heads/${branch}`], worktree);
  if (currentHead !== exactHead || branchHead !== exactHead) {
    throw new Error("Submitted exactHead is not the checked-out private team branch head");
  }
  if (await spawnCheckedRaw(["git", "status", "--porcelain=v1", "-z"], worktree)) {
    throw new Error("Team worktree contains uncommitted changes");
  }
  await spawnChecked(["git", "merge-base", "--is-ancestor", base, exactHead], worktree);
  await spawnChecked(["git", "fsck", "--strict", "--no-dangling"], worktree);
  const actualPaths = await changedPathsSince(worktree, base, exactHead);
  const reportedPaths = normalizedPaths(data.changedPaths);
  if (JSON.stringify(actualPaths) !== JSON.stringify(reportedPaths)) {
    throw new Error(`Reported changedPaths do not match Git: expected ${JSON.stringify(actualPaths)}`);
  }
  const targetRef = reviewRef(String(work.id));
  await spawnChecked(["git", "check-ref-format", targetRef]);
  await spawnChecked(["git", "fetch", "--no-tags", "--force", worktree, `+refs/heads/${branch}:${targetRef}`]);
  const importedHead = await spawnChecked(["git", "rev-parse", targetRef]);
  if (importedHead !== exactHead) throw new Error("Imported review ref does not match submitted exactHead");
}

function githubRepository(remote: string): string {
  const match = /^https:\/\/github\.com\/([^/]+\/[^/]+?)(?:\.git)?$/.exec(remote);
  if (!match?.[1]) throw new Error(`Unsupported origin remote: ${remote}`);
  return match[1];
}

async function defaultBranch(repository: string): Promise<string> {
  const repo = JSON.parse(await spawnChecked([
    "gh", "repo", "view", repository, "--json", "defaultBranchRef",
  ])) as { defaultBranchRef?: { name?: string } };
  const branch = repo.defaultBranchRef?.name;
  if (!branch) throw new Error(`GitHub did not report a default branch for ${repository}`);
  return branch;
}

async function verifiedOrigin(config: FleetConfig): Promise<{ remote: string; repository: string; baseBranch: string }> {
  const remote = await spawnChecked(["git", "remote", "get-url", "origin"]);
  const repository = githubRepository(remote);
  if (repository !== config.repo) throw new Error(`Origin ${repository} does not match configured repository ${config.repo}`);
  return { remote, repository, baseBranch: await defaultBranch(repository) };
}

async function prepareHandoff(config: FleetConfig, work: Record<string, unknown>, data: Record<string, unknown>): Promise<void> {
  const exactHead = String(work.exact_head);
  const branch = String(work.branch);
  const worktree = String(work.worktree);
  if (await spawnChecked(["git", "rev-parse", `refs/heads/${branch}`], worktree) !== exactHead) {
    throw new Error("Private team branch moved after independent review");
  }
  const { remote, repository, baseBranch } = await verifiedOrigin(config);
  const existing = await spawnCheckedRaw(["git", "ls-remote", "--heads", remote, `refs/heads/${branch}`]);
  if (existing && !existing.startsWith(`${exactHead}\t`)) {
    throw new Error(`Remote branch ${branch} exists at a different head`);
  }
  if (!existing) {
    await spawnChecked(["git", "push", remote, `${exactHead}:refs/heads/${branch}`], worktree);
  }
  const remoteHead = await spawnCheckedRaw(["git", "ls-remote", "--heads", remote, `refs/heads/${branch}`]);
  if (!remoteHead.startsWith(`${exactHead}\t`)) throw new Error("Remote head does not match reviewed exact head");

  let pullRequest = typeof data.pullRequest === "string" ? data.pullRequest : "";
  if (!pullRequest) {
    const open = JSON.parse(await spawnChecked([
      "gh", "pr", "list", "--repo", repository, "--state", "open", "--head", branch,
      "--json", "url,headRefOid,baseRefName",
    ])) as Array<{ url: string; headRefOid: string; baseRefName: string }>;
    if (open.length > 1) throw new Error(`Multiple open PRs found for ${branch}`);
    pullRequest = open[0]?.url ?? await spawnChecked([
      "gh", "pr", "create",
      "--repo", repository,
      "--head", branch,
      "--base", baseBranch,
      "--title", String(work.principal_goal),
      "--body", `Fleet work ${work.id}\n\nReviewed exact head: \`${exactHead}\``,
    ]);
  }
  const pr = JSON.parse(await spawnChecked([
    "gh", "pr", "view", pullRequest, "--repo", repository,
    "--json", "url,state,headRefOid,baseRefName",
  ])) as { url: string; state: string; headRefOid: string; baseRefName: string };
  if (pr.state !== "OPEN" || pr.headRefOid !== exactHead || pr.baseRefName !== baseBranch) {
    throw new Error("Pull request does not expose the reviewed exact head against the configured base");
  }
  data.remoteHead = exactHead;
  data.pullRequest = pr.url;
}

async function verifyMergedHandoff(config: FleetConfig, work: Record<string, unknown>): Promise<void> {
  const { repository, baseBranch } = await verifiedOrigin(config);
  const pr = JSON.parse(await spawnChecked([
    "gh", "pr", "view", String(work.pull_request), "--repo", repository,
    "--json", "state,mergedAt,headRefOid,baseRefName",
  ])) as { state: string; mergedAt: string | null; headRefOid: string; baseRefName: string };
  if (pr.state !== "MERGED" || !pr.mergedAt || pr.headRefOid !== work.exact_head || pr.baseRefName !== baseBranch) {
    throw new Error("Protected PR merge evidence does not match the handed-off exact head");
  }
}

async function retireAgent(config: FleetConfig, runtime: RuntimePaths, store: FleetStore, identity: string): Promise<string[]> {
  const failures: string[] = [];
  const placement = store.agentPlacement(identity);
  try {
    store.revokeAgentToken(identity);
  } catch (error) {
    failures.push(`token: ${error instanceof Error ? error.message : String(error)}`);
  }
  const stop = Bun.spawn(["herdr", "--session", config.session, "agent", "send-keys", identity.toLowerCase(), "CTRL_C"], {
    stdout: "ignore",
    stderr: "ignore",
  });
  if (await stop.exited !== 0) failures.push("agent interrupt failed");
  if (typeof placement.workspace_id === "string") {
    try {
      await removePlacement(config, identity, { workspaceId: placement.workspace_id });
    } catch (error) {
      failures.push(`placement: ${error instanceof Error ? error.message : String(error)}`);
    }
  }
  store.releaseAgent(identity);
  rmSync(join(runtime.runtimeDir, "agents", identity), { recursive: true, force: true });
  return failures;
}

function recordCleanupFailures(store: FleetStore, requestId: string, failures: string[]): void {
  if (failures.length === 0) return;
  store.db.query("INSERT INTO events(at,actor_id,kind,payload_json) VALUES(?,?,?,?)")
    .run(new Date().toISOString(), "Dispatcher", "cleanup.failed", JSON.stringify({ requestId, failures }));
}

async function retireWorkAgents(
  config: FleetConfig,
  runtime: RuntimePaths,
  store: FleetStore,
  work: Record<string, unknown>,
): Promise<string[]> {
  const failures: string[] = [];
  for (const identity of [String(work.team_id ?? ""), config.reviewer.id]) {
    if (!identity) continue;
    const placement = store.agentPlacement(identity);
    if (placement.current_work_id !== work.id || typeof placement.workspace_id !== "string") continue;
    failures.push(...(await retireAgent(config, runtime, store, identity)).map((failure) => `${identity}: ${failure}`));
  }
  return failures;
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
  let actor: ReturnType<FleetStore["authenticate"]>;
  try {
    actor = store.authenticate(request.token);
    if (typeof request.id !== "string" || request.id === "") throw new Error("id must be a non-empty string");
    const cached = store.cachedResponse(request, actor.id);
    if (cached) return cached;
    const data = request.data ?? (request.data = {});

    if (request.op === "launch_team" || request.op === "launch_reviewer") {
      if (actor.role !== "main") throw new Error("Only Main can launch fleet agents");
      const workId = String(data.workId ?? "");
      const result = await launchAgent(config, runtime, store, workId, request.op === "launch_team" ? "team" : "reviewer");
      const response = store.recordApplied(request, result);
      await mirrorEvent(store, request, response);
      return response;
    }

    let work = request.op !== "plan" && typeof data.workId === "string" ? store.getWork(data.workId) : undefined;
    if (request.op === "plan") {
      if (actor.role !== "main") throw new Error("Only Main can create work");
      const origin = await spawnChecked(["git", "remote", "get-url", "origin"]);
      const repository = githubRepository(origin);
      const declaredRepository = typeof data.repository === "string" ? data.repository : repository;
      if (![repository, `https://github.com/${repository}`, `https://github.com/${repository}.git`].includes(declaredRepository)) {
        throw new Error(`repository must identify the active origin ${repository}`);
      }
      const baseBranch = typeof data.baseBranch === "string" ? data.baseBranch : await defaultBranch(repository);
      await spawnChecked(["git", "check-ref-format", "--branch", baseBranch]);
      const remoteLine = await spawnCheckedRaw(["git", "ls-remote", origin, `refs/heads/${baseBranch}`]);
      const verifiedBaseSha = remoteLine.split(/\s+/, 1)[0] ?? "";
      if (!/^[0-9a-f]{40}$/.test(verifiedBaseSha)) {
        throw new Error(`Origin did not expose a valid ${baseBranch} head`);
      }
      if (typeof data.verifiedBaseSha === "string" && data.verifiedBaseSha !== verifiedBaseSha) {
        throw new Error(`verifiedBaseSha does not match origin ${baseBranch} at ${verifiedBaseSha}`);
      }
      data.repository = repository;
      data.verifiedBaseSha = verifiedBaseSha;
    } else if (request.op === "submit") {
      if (!work || actor.role !== "team" || work.team_id !== actor.teamId) {
        throw new Error("Only the assigned Feature Team can submit this work item");
      }
      await validateAndImportSubmission(work, data);
    } else if (request.op === "handoff") {
      if (!work || actor.role !== "main" || work.state !== "handoff_ready") {
        throw new Error("Only Main can hand off approved work");
      }
      await prepareHandoff(config, work, data);
    } else if (request.op === "finish") {
      if (!work || actor.role !== "main" || work.state !== "handed_off") {
        throw new Error("Only Main can finish handed-off work");
      }
      await verifyMergedHandoff(config, work);
      data.exactHead = work.exact_head;
      data.pullRequest = work.pull_request;
    }

    const response = store.handle(request);
    if (!response.ok) return response;
    if (typeof data.workId === "string") work = store.getWork(data.workId);
    if (work) {
      if (request.op === "review") {
        recordCleanupFailures(store, request.id, await retireAgent(config, runtime, store, config.reviewer.id));
        rmSync(reviewWorktree(runtime, work), { recursive: true, force: true });
      } else if (["handoff", "cancel", "finish"].includes(request.op)) {
        recordCleanupFailures(store, request.id, await retireWorkAgents(config, runtime, store, work));
        if (request.op === "finish") {
          await spawnChecked(["git", "update-ref", "-d", reviewRef(String(work.id))]);
          rmSync(String(work.worktree), { recursive: true, force: true });
        }
      }
    }
    await mirrorEvent(store, request, response);
    return response;
  } catch (error) {
    try {
      store.recordRejected(request, error);
    } catch {
      // Authentication and duplicate-id failures cannot be attributed safely.
    }
    return { ok: false, requestId: request.id || "unknown", error: error instanceof Error ? error.message : String(error) };
  }
}

async function serve(config: FleetConfig, runtime: RuntimePaths): Promise<void> {
  mkdirSync(runtime.runtimeDir, { recursive: true, mode: 0o700 });
  rmSync(runtime.socket, { force: true });
  const store = new FleetStore(runtime.database, runtime.runtimeDir);
  store.bootstrap(config);
  await reconcilePlacements(config, store);
  const credentialProxy = startCredentialProxy(
    config.credentialProxy,
    (token) => store.credentialAssignment(token),
    runtime.credentialSocket,
  );
  let requestQueue = Promise.resolve();
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
      requestQueue = requestQueue.then(async () => {
        try {
          const request = JSON.parse(line) as FleetRequest;
          const response = await processRequest(config, runtime, store, request);
          socket.end(`${JSON.stringify(response)}\n`);
        } catch (error) {
          socket.end(`${JSON.stringify({
            ok: false,
            requestId: "unknown",
            error: error instanceof Error ? error.message : String(error),
          })}\n`);
        }
      });
    });
  });
  server.listen(runtime.socket, () => chmodSync(runtime.socket, 0o600));
  const shutdown = () => {
    server.close();
    credentialProxy.stop(true);
    store.close();
    rmSync(runtime.socket, { force: true });
    rmSync(runtime.credentialSocket, { force: true });
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
  if (command === "launch-main") {
    const store = new FleetStore(runtime.database, runtime.runtimeDir);
    store.bootstrap(config);
    try {
      console.log(JSON.stringify({ ok: true, result: await launchMain(config, runtime, store) }));
    } finally {
      store.close();
    }
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
    const socket = existsSync(runtime.socket);
    const ok = schema === FLEET_SCHEMA_VERSION && socket;
    console.log(JSON.stringify({ ok, socket, schema }));
    if (!ok) process.exitCode = 1;
    return;
  }
  console.error("Usage: dispatcher.ts [--config path] serve|bootstrap|launch-main|request|health");
  process.exitCode = 2;
}

if (import.meta.main) await main();
