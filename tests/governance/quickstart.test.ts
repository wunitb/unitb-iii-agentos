import { describe, expect, it } from "bun:test";

const repository = new URL("../", import.meta.url);

describe("README bootstrap quickstart", () => {
  it("documents build, up, no-TUI, and doctor as the supported flow", async () => {
    const readme = await Bun.file(new URL("README.md", repository)).text();

    expect(readme).toContain("cargo build --workspace --release");
    expect(readme).toContain("./target/release/agentos up");
    expect(readme).toContain("agentos up --no-tui");
    expect(readme).toContain("agentos doctor");
    expect(readme).toContain("The TUI opens on Chat");
  });
});
