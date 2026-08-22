import { describe, expect, it } from "bun:test";

const repository = new URL("../", import.meta.url);

describe("TUI provider surfaces", () => {
  it("keeps the registered Sessions and Security HTTP providers", async () => {
    const memory = await Bun.file(
      new URL("workers/memory/src/main.rs", repository),
    ).text();
    const security = await Bun.file(
      new URL("workers/security/src/main.rs", repository),
    ).text();

    expect(memory).toContain(
      '("memory::session::list", "GET", "/api/sessions")',
    );
    expect(security).toContain(
      '("security::list_capabilities", "GET", "/api/security")',
    );
  });

  it("does not call routes that deliberately have no provider", async () => {
    const tui = await Bun.file(
      new URL("crates/tui/src/main.rs", repository),
    ).text();

    for (const route of [
      "/api/dashboard/stats",
      "/api/dashboard/logs",
      "/api/dashboard/events",
      "/api/settings",
    ]) {
      expect(tui).toContain(`GET ${route}`);
      expect(tui).not.toContain(`format!("{}${route}", API_BASE)`);
    }
    expect(tui).toContain("No provider");
  });
});
