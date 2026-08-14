import { existsSync } from "node:fs";
import { join } from "node:path";

interface ExtensionContext {
  cwd: string;
}


interface ExtensionAPI {
  on(
    event: string,
    handler: (event: Record<string, unknown>, context: ExtensionContext) => Promise<unknown>,
  ): void;
}

const MAIN_TOOLS: Record<string, true> = {
  read: true,
  grep: true,
  glob: true,
  lsp: true,
  todo: true,
  ask: true,
  fleet_plan: true,
  fleet_assign: true,
  fleet_launch_team: true,
  fleet_status: true,
  fleet_message: true,
  fleet_review: true,
  fleet_handoff: true,
  fleet_cancel: true,
  fleet_resume: true,
  fleet_finish: true,
};

const TEAM_TOOLS: Record<string, true> = {
  read: true,
  grep: true,
  glob: true,
  lsp: true,
  bash: true,
  edit: true,
  write: true,
  python: true,
  notebook: true,
  inspect_image: true,
  browser: true,
  todo: true,
  ask: true,
  fleet_ack: true,
  fleet_report: true,
  fleet_submit: true,
  fleet_status: true,
  fleet_message: true,
};

const REVIEWER_TOOLS: Record<string, true> = {
  read: true,
  grep: true,
  glob: true,
  lsp: true,
  bash: true,
  todo: true,
  ask: true,
  fleet_report: true,
  fleet_review: true,
  fleet_status: true,
  fleet_message: true,
};

const READ_ONLY_LSP_ACTIONS: Record<string, true> = {
  diagnostics: true,
  definition: true,
  references: true,
  hover: true,
  symbols: true,
  type_definition: true,
  implementation: true,
  status: true,
  capabilities: true,
  reload: true,
};

function fleetConfigExists(cwd: string): boolean {
  const explicit = process.env.UNITB_FLEET_CONFIG;
  if (explicit) return existsSync(explicit);
  return [
    join(cwd, "orchestration", "fleet.config.json"),
    join(cwd, "unitb-iii-agentos", "orchestration", "fleet.config.json"),
  ].some(existsSync);
}

function roleFor(cwd: string): "main" | "team" | "reviewer" | undefined {
  const explicit = process.env.UNITB_FLEET_ROLE;
  if (explicit === "main" || explicit === "team" || explicit === "reviewer") return explicit;
  return fleetConfigExists(cwd) ? "main" : undefined;
}

function blockedPath(input: Record<string, unknown>): boolean {
  const path = typeof input.path === "string" ? input.path : "";
  return path.startsWith("xd://") || path.startsWith("agent://") || path.startsWith("history://");
}

function stringArray(value: unknown, name: string): string[] {
  if (!Array.isArray(value) || !value.every((item) => typeof item === "string")) {
    throw new Error(`${name} must be an array of strings`);
  }
  return value;
}

export default function unitbFleetRoleGuard(pi: ExtensionAPI): void {
  pi.on("tool_call", async (event, ctx) => {
    const role = roleFor(ctx.cwd);
    if (!role) return undefined;
    const toolName = String(event.toolName ?? "");
    const input = event.input && typeof event.input === "object"
      ? event.input as Record<string, unknown>
      : {};
    const allowed = role === "main" ? MAIN_TOOLS : role === "team" ? TEAM_TOOLS : REVIEWER_TOOLS;
    if (!allowed[toolName]) {
      return { block: true, reason: `${role} role cannot use ${toolName}` };
    }
    if (toolName === "lsp") {
      const action = String(input.action ?? "");
      if (role !== "team" && !READ_ONLY_LSP_ACTIONS[action]) {
        return { block: true, reason: `${role} role cannot use mutating LSP action ${action}` };
      }
    }
    if ((role === "team" || role === "reviewer") && blockedPath(input)) {
      return { block: true, reason: `${role} role cannot access host tool devices` };
    }
    return undefined;
  });

  pi.on("before_agent_start", async (event, ctx) => {
    const role = roleFor(ctx.cwd);
    if (!role) return undefined;
    const identity = process.env.UNITB_FLEET_ID ?? (role === "main" ? "Main" : role);
    const contract = role === "main"
      ? "Read-only control plane. Decompose, assign, monitor, review-gate, and hand off through fleet tools. Never implement, commit, push, merge, or spawn subagents."
      : role === "team"
        ? "Single-writer worker. Edit only assigned owned paths in this worktree. Never spawn subagents, push, merge, or bypass the Dispatcher."
        : "Independent reviewer. Review only the exact submitted SHA in this disposable worktree. Never change the source branch, push, merge, or resolve your own findings.";
    const systemPrompt = stringArray(event.systemPrompt, "systemPrompt");
    return { systemPrompt: [...systemPrompt, `<unitb-fleet-role identity="${identity}" role="${role}">${contract}</unitb-fleet-role>`] };
  });
}
