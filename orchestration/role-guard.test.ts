import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import unitbFleetRoleGuard from "./role-guard";

type Hook = (event: Record<string, unknown>, context: { cwd: string }) => Promise<unknown>;

const roots: string[] = [];

afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

function mainGuard(): { cwd: string; hooks: Record<string, Hook> } {
  const cwd = join(tmpdir(), `unitb-role-guard-${crypto.randomUUID()}`);
  mkdirSync(join(cwd, "orchestration"), { recursive: true });
  writeFileSync(join(cwd, "orchestration", "fleet.config.json"), "{}\n");
  roots.push(cwd);

  const hooks: Record<string, Hook> = {};
  unitbFleetRoleGuard({
    on(name: string, hook: Hook) {
      hooks[name] = hook;
    },
  });
  return { cwd, hooks };
}

describe("fleet role guard", () => {
  test("Main denies every direct mutation surface", async () => {
    const { cwd, hooks } = mainGuard();
    for (const toolName of ["bash", "edit", "write", "python", "notebook", "browser", "task", "web_search"]) {
      expect(await hooks.tool_call({ toolName, input: {} }, { cwd })).toEqual({
        block: true,
        reason: `main role cannot use ${toolName}`,
      });
    }
  });

  test("Main allows control-plane tools and read-only LSP actions", async () => {
    const { cwd, hooks } = mainGuard();
    expect(await hooks.tool_call({ toolName: "fleet_status", input: {} }, { cwd })).toBeUndefined();
    expect(await hooks.tool_call({ toolName: "lsp", input: { action: "references" } }, { cwd })).toBeUndefined();
    expect(await hooks.tool_call({ toolName: "lsp", input: { action: "rename" } }, { cwd })).toEqual({
      block: true,
      reason: "main role cannot use mutating LSP action rename",
    });
  });
});
