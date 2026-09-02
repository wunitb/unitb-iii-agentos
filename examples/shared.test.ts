import type { IIIClient, JsonValue } from "iii-sdk";
import { describe, expect, it, vi } from "vitest";
import { agentStateWrites, registerHttpTrigger } from "./shared.js";

type Handler = (input: JsonValue) => Promise<JsonValue>;

function createClient(result: JsonValue) {
  let handler: Handler | undefined;
  const trigger = vi.fn().mockResolvedValue(result);
  const registerTrigger = vi.fn();
  const registerFunction = vi.fn((_id: string, candidate: Handler) => {
    handler = candidate;
    return {};
  });
  const client = {
    registerFunction,
    registerTrigger,
    trigger,
  } as unknown as IIIClient;

  return {
    client,
    getHandler: () => {
      if (!handler) throw new Error("HTTP adapter handler was not registered");
      return handler;
    },
    registerTrigger,
    trigger,
  };
}

describe("registerHttpTrigger", () => {
  it("normalizes iii HTTP request envelopes before internal invocation", async () => {
    const { client, getHandler, registerTrigger, trigger } = createClient({
      ok: true,
    });

    registerHttpTrigger(client, "crew::demo", {
      api_path: "crew/:crewId",
      http_method: "post",
    });

    const response = await getHandler()({
      body: { topic: "runtime", limit: 20, items: [1] },
      query_params: { limit: ["10"], tag: ["a", "b"] },
      path_params: { crewId: "crew-7" },
      headers: { authorization: "Bearer token" },
    });

    expect(trigger).toHaveBeenCalledWith({
      function_id: "crew::demo",
      payload: {
        body: { topic: "runtime", limit: 20, items: [1] },
        query_params: { limit: ["10"], tag: ["a", "b"] },
        path_params: { crewId: "crew-7" },
        headers: { authorization: "Bearer token" },
        topic: "runtime",
        limit: 20,
        items: [1],
        tag: ["a", "b"],
        query: { limit: ["10"], tag: ["a", "b"] },
        crewId: "crew-7",
      },
    });
    expect(response).toEqual({ status_code: 200, body: { ok: true } });
    expect(registerTrigger).toHaveBeenCalledWith({
      type: "http",
      function_id: "agentos::http::crew::demo::POST::/crew/:crewId",
      config: { api_path: "/crew/:crewId", http_method: "POST" },
    });
  });

  it("preserves explicit HTTP response envelopes", async () => {
    const explicitResponse = {
      status_code: 201,
      headers: { location: "/crew/7" },
      body: { id: 7 },
    };
    const { client, getHandler } = createClient(explicitResponse);

    registerHttpTrigger(client, "crew::create", {
      api_path: "/crew",
      http_method: "POST",
    });

    await expect(getHandler()({ body: { name: "seven" } })).resolves.toEqual(
      explicitResponse,
    );
  });
});

describe("agentStateWrites", () => {
  it("puts the agent id inside the document because state::list drops keys", () => {
    const writes = agentStateWrites(
      "researcher",
      { name: "Researcher", capabilities: { tools: ["tool::*"] } },
      1_700_000_000_000,
    );

    const agents = writes.find((w) => w.scope === "agents");
    expect(agents).toBeDefined();
    expect(agents!.key).toBe("researcher");
    expect(agents!.value).toEqual({
      name: "Researcher",
      capabilities: { tools: ["tool::*"] },
      id: "researcher",
    });
  });

  it("writes the capability document to the scope the reader uses", () => {
    const writes = agentStateWrites(
      "researcher",
      { capabilities: { tools: ["memory::*", "workflow::run"] } },
      1_700_000_000_000,
    );

    const capabilities = writes.find((w) => w.scope === "capabilities");
    expect(capabilities).toBeDefined();
    expect(capabilities!.key).toBe("researcher");
    expect(capabilities!.value).toEqual({
      tools: ["memory::*", "workflow::run"],
      updatedAt: 1_700_000_000_000,
    });
  });

  it("grants no tool when the config declares none", () => {
    const writes = agentStateWrites("plain", { name: "Plain" }, 1);
    const capabilities = writes.find((w) => w.scope === "capabilities");
    expect(capabilities!.value).toEqual({ tools: [], updatedAt: 1 });
  });

  it("keeps every write in the state::set shape the engine accepts", () => {
    for (const write of agentStateWrites("a", { name: "A" }, 1)) {
      expect(Object.keys(write).sort()).toEqual(["key", "scope", "value"]);
      expect(typeof write.scope).toBe("string");
      expect(typeof write.key).toBe("string");
    }
  });
});
