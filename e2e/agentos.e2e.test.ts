import { describe, expect, it } from "vitest";

const shouldRunE2E = process.env.AGENTOS_E2E === "1";
const suite = shouldRunE2E ? describe : describe.skip;

const baseUrl = process.env.AGENTOS_BASE_URL || "http://127.0.0.1:3111";
const chatModel = process.env.AGENTOS_E2E_MODEL || "gpt-5.6-sol";
const apiKey = process.env.AGENTOS_API_KEY || "";

function authHeaders(): Record<string, string> {
  return {
    ...(apiKey ? { authorization: `Bearer ${apiKey}` } : {}),
    "content-type": "application/json",
  };
}

suite("AgentOS E2E", () => {
  it("health endpoint reports the live AgentOS runtime", async () => {
    const response = await fetch(`${baseUrl}/api/health`);
    expect(response.status).toBe(200);
    const body: any = await response.json();
    expect(body?.status).toBe("healthy");
    expect(typeof body?.version).toBe("string");
    expect(body?.workers).toBeGreaterThan(0);
    expect(body?.uptime).toBeGreaterThanOrEqual(0);
  });

  it("chat_completions endpoint responds with valid shape", async () => {
    const response = await fetch(`${baseUrl}/v1/chat/completions`, {
      method: "POST",
      headers: authHeaders(),
      body: JSON.stringify({
        model: chatModel,
        messages: [{ role: "user", content: "Reply with the word READY only." }],
      }),
    });

    expect(response.status).toBe(200);
    const body: any = await response.json();
    expect(body?.object).toBe("chat.completion");
    expect(Array.isArray(body?.choices)).toBe(true);
    expect(typeof body?.choices?.[0]?.message?.content).toBe("string");
  });
});
