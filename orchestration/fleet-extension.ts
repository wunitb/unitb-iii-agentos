import { existsSync, readFileSync } from "node:fs";
import { createConnection } from "node:net";
import { dirname, join, resolve } from "node:path";

interface Schema {
  min(value: number): Schema;
  max(value: number): Schema;
  optional(): Schema;
  regex(pattern: RegExp): Schema;
  url(): Schema;
}

interface SchemaFactory {
  array(schema: Schema): Schema;
  enum(values: readonly string[]): Schema;
  literal(value: string): Schema;
  object(shape: Record<string, Schema>): Schema;
  string(): Schema;
}

interface ExtensionContext {
  hasUI: boolean;
  ui: {
    notify(message: string, level: string): void;
  };
}

interface ToolDefinition {
  execute(id: string, params: Record<string, unknown>): Promise<unknown>;
  [key: string]: unknown;
}

interface ExtensionAPI {
  zod: SchemaFactory;
  on(event: "session_start", handler: (event: unknown, context: ExtensionContext) => unknown): void;
  registerTool(tool: ToolDefinition): void;
}

interface ExtensionConfig {
  configPath: string;
  repoRoot: string;
  runtimeDir: string;
  worktreeDir: string;
  role: "main" | "team" | "reviewer";
  identity: string;
  socketPath: string;
  tokenFile: string;
}

interface FleetResponse {
  ok: boolean;
  requestId: string;
  result?: unknown;
  error?: string;
}

function locateConfig(cwd: string): string | undefined {
  const explicit = process.env.UNITB_FLEET_CONFIG;
  if (explicit && existsSync(explicit)) return explicit;
  const candidates = [
    join(cwd, "orchestration", "fleet.config.json"),
    join(cwd, "unitb-iii-agentos", "orchestration", "fleet.config.json"),
  ];
  return candidates.find(existsSync);
}

function extensionConfig(cwd: string): ExtensionConfig | undefined {
  const configPath = locateConfig(cwd);
  if (!configPath) return undefined;
  const config = JSON.parse(readFileSync(configPath, "utf8")) as {
    runtimeDir: string;
    worktreeDir: string;
    reviewer: { id: string };
  };
  const repoRoot = resolve(dirname(configPath), "..");
  const runtimeDir = resolve(repoRoot, config.runtimeDir);
  const worktreeDir = resolve(repoRoot, config.worktreeDir);
  const role = (process.env.UNITB_FLEET_ROLE as ExtensionConfig["role"] | undefined) ?? "main";
  const identity = process.env.UNITB_FLEET_ID ?? (role === "main" ? "Main" : role === "reviewer" ? config.reviewer.id : "");
  if (!identity) return undefined;
  return {
    configPath,
    repoRoot,
    runtimeDir,
    worktreeDir,
    role,
    identity,
    socketPath: process.env.UNITB_FLEET_SOCKET ?? join(runtimeDir, "dispatcher.sock"),
    tokenFile: process.env.UNITB_FLEET_TOKEN_FILE ?? join(runtimeDir, "tokens", `${identity}.token`),
  };
}

function request(config: ExtensionConfig, op: string, data: Record<string, unknown>, id = crypto.randomUUID()): Promise<FleetResponse> {
  const { promise, resolve: resolveResponse, reject } = Promise.withResolvers<FleetResponse>();
  if (!existsSync(config.tokenFile)) {
    reject(new Error(`Fleet token not found: ${config.tokenFile}`));
    return promise;
  }
  const socket = createConnection(config.socketPath);
  const token = readFileSync(config.tokenFile, "utf8").trim();
  let buffer = "";
  const timeout = setTimeout(() => {
    socket.destroy();
    reject(new Error("Fleet Dispatcher timeout"));
  }, 120_000);
  socket.setEncoding("utf8");
  socket.on("connect", () => socket.write(`${JSON.stringify({ id, op, token, data })}\n`));
  socket.on("data", (chunk) => {
    buffer += chunk;
    const newline = buffer.indexOf("\n");
    if (newline < 0) return;
    clearTimeout(timeout);
    socket.end();
    const response = JSON.parse(buffer.slice(0, newline)) as FleetResponse;
    if (!response.ok) reject(new Error(response.error ?? `Fleet operation ${op} failed`));
    else resolveResponse(response);
  });

  socket.on("error", (error) => {
    clearTimeout(timeout);
    reject(error);
  });
  return promise;
}

function stringParam(params: Record<string, unknown>, name: string): string {
  const value = params[name];
  if (typeof value !== "string" || value.length === 0) throw new Error(`${name} must be a non-empty string`);
  return value;
}

function toolResult(response: FleetResponse) {
  return {
    content: [{ type: "text" as const, text: JSON.stringify(response.result ?? {}, null, 2) }],
    details: { requestId: response.requestId, result: response.result },
  };
}

export default function unitbFleet(pi: ExtensionAPI): void {
  const config = extensionConfig(process.cwd());
  if (!config) return;

  pi.on("session_start", (_event, ctx) => {
    if (ctx.hasUI) ctx.ui.notify(`UnitB fleet role: ${config.identity} (${config.role})`, "info");
  });

  const active = () => config;

  pi.registerTool({
    name: "fleet_status",
    loadMode: "essential",
    label: "fleet.status",
    description: "Read durable fleet work, agent, and queued-message state from the Dispatcher.",
    parameters: pi.zod.object({ workId: pi.zod.string().min(1).optional() }),
    async execute(_id, params) {
      return toolResult(await request(active(), "status", params));
    },
  });

  pi.registerTool({
    name: "fleet_message",
    loadMode: "essential",
    label: "fleet.message",
    description: "Send a durable message through the Dispatcher; never use raw pane input for coordination.",
    parameters: pi.zod.object({
      recipient: pi.zod.string().min(1),
      body: pi.zod.string().min(1).max(8_000),
      workId: pi.zod.string().min(1).optional(),
      messageId: pi.zod.string().min(1).optional(),
    }),
    async execute(_id, params) {
      return toolResult(await request(active(), "message", { ...params, messageId: params.messageId ?? crypto.randomUUID() }));
    },
  });

  pi.registerTool({
    name: "fleet_report",
    loadMode: "essential",
    label: "fleet.report",
    description: "Record progress, evidence, or a blocked state for an assigned work item.",
    parameters: pi.zod.object({
      workId: pi.zod.string().min(1),
      status: pi.zod.string().min(1),
      reason: pi.zod.string().min(1).optional(),
      evidence: pi.zod.array(pi.zod.string()).optional(),
    }),
    async execute(_id, params) {
      return toolResult(await request(active(), "report", params));
    },
  });

  if (config.role === "main") {
      pi.registerTool({
        name: "fleet_plan",
        loadMode: "essential",
        label: "fleet.plan",
        description: "Create one durable work item from the Principal's verbatim goal and a verified base SHA.",
        parameters: pi.zod.object({
          workId: pi.zod.string().min(1),
          principalGoal: pi.zod.string().min(1),
          repository: pi.zod.string().min(1),
          verifiedBaseSha: pi.zod.string().regex(/^[0-9a-f]{40}$/),
        }),
        async execute(_id, params) {
          return toolResult(await request(active(), "plan", params));
        },
      });

      pi.registerTool({
        name: "fleet_assign",
        loadMode: "essential",
        label: "fleet.assign",
        description: "Assign an owned path set to exactly one Team and launch its constrained persistent OMP worker.",
        parameters: pi.zod.object({
          workId: pi.zod.string().min(1),
          teamId: pi.zod.string().min(1),
          contract: pi.zod.object({
            goal: pi.zod.string().min(1),
            ownedPaths: pi.zod.array(pi.zod.string().min(1)).min(1),
            readOnlyPaths: pi.zod.array(pi.zod.string().min(1)).optional(),
            forbiddenPaths: pi.zod.array(pi.zod.string().min(1)).optional(),
            dependsOn: pi.zod.array(pi.zod.string().min(1)).optional(),
            nonGoals: pi.zod.array(pi.zod.string().min(1)).optional(),
            acceptance: pi.zod.array(pi.zod.string().min(1)).min(1),
            verification: pi.zod.array(pi.zod.string().min(1)).min(1),
            mergeAuthority: pi.zod.literal("principal"),
          }),
        }),
        async execute(_id, params) {
          const currentConfig = active();
          const teamId = stringParam(params, "teamId");
          const workId = stringParam(params, "workId");
          const worktree = join(currentConfig.worktreeDir, teamId, workId);
          const assignment = await request(currentConfig, "assign", { ...params, worktree });
          const launch = await request(currentConfig, "launch_team", { workId });
          return toolResult({ ok: true, requestId: assignment.requestId, result: { assignment: assignment.result, launch: launch.result } });
        },
      });

      pi.registerTool({
        name: "fleet_review",
        loadMode: "essential",
        label: "fleet.review",
        description: "Launch the independent Reviewer against the Team's exact submitted commit.",
        parameters: pi.zod.object({ workId: pi.zod.string().min(1) }),
        async execute(_id, params) {
          return toolResult(await request(active(), "launch_reviewer", params));
        },
      });

      pi.registerTool({
        name: "fleet_handoff",
        loadMode: "essential",
        label: "fleet.handoff",
        description: "Record an approved exact-head PR handoff to the Principal; this never merges.",
        parameters: pi.zod.object({
          workId: pi.zod.string().min(1),
          exactHead: pi.zod.string().regex(/^[0-9a-f]{40}$/),
          remoteHead: pi.zod.string().regex(/^[0-9a-f]{40}$/),
          pullRequest: pi.zod.string().url(),
        }),
        async execute(_id, params) {
          return toolResult(await request(active(), "handoff", params));
        },
      });

      pi.registerTool({
        name: "fleet_cancel",
        loadMode: "essential",
        label: "fleet.cancel",
        description: "Cancel one non-terminal work item and interrupt its Team while preserving its worktree and evidence.",
        parameters: pi.zod.object({ workId: pi.zod.string().min(1), reason: pi.zod.string().min(1) }),
        async execute(_id, params) {
          return toolResult(await request(active(), "cancel", params));
        },
      });
  } else if (config.role === "team") {
      pi.registerTool({
        name: "fleet_ack",
        loadMode: "essential",
        label: "fleet.ack",
        description: "Acknowledge the exact assignment contract revision before implementation begins.",
        parameters: pi.zod.object({ workId: pi.zod.string().min(1), contractRevision: pi.zod.string().min(1) }),
        async execute(_id, params) {
          return toolResult(await request(active(), "ack", params));
        },
      });

      pi.registerTool({
        name: "fleet_submit",
        loadMode: "essential",
        label: "fleet.submit",
        description: "Submit an exact committed head, owned changed paths, and verification evidence for review.",
        parameters: pi.zod.object({
          workId: pi.zod.string().min(1),
          exactHead: pi.zod.string().regex(/^[0-9a-f]{40}$/),
          changedPaths: pi.zod.array(pi.zod.string().min(1)).min(1),
          verification: pi.zod.array(pi.zod.string().min(1)).min(1),
        }),
        async execute(_id, params) {
          return toolResult(await request(active(), "submit", params));
        },
      });
  } else {
      pi.registerTool({
        name: "fleet_review",
        loadMode: "essential",
        label: "fleet.review",
        description: "Submit an independent verdict for the exact reviewed commit.",
        parameters: pi.zod.object({
          workId: pi.zod.string().min(1),
          exactHead: pi.zod.string().regex(/^[0-9a-f]{40}$/),
          verdict: pi.zod.enum(["approved", "changes_requested"]),
          findings: pi.zod.array(pi.zod.string()).optional(),
        }),
        async execute(_id, params) {
          return toolResult(await request(active(), "review", params));
        },
      });
    }
}
