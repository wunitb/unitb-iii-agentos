import { describe, expect, it } from "bun:test";

const repository = new URL("../../", import.meta.url);

describe("README bootstrap quickstart", () => {
  it("documents build, up, no-TUI, and doctor as the supported flow", async () => {
    const readme = await Bun.file(new URL("README.md", repository)).text();

    expect(readme).toContain("cargo build --workspace --release");
    expect(readme).toContain("./target/release/agentos up");
    expect(readme).toContain("agentos up --no-tui");
    expect(readme).toContain("agentos doctor");
    expect(readme).toContain("The TUI opens on Chat");
  });

  it("documents the first-run key contract that `up` and `onboard` implement", async () => {
    const readme = await Bun.file(new URL("README.md", repository)).text();
    const section = readme.slice(
      readme.indexOf("### First run — what generates what"),
      readme.indexOf("### Installed releases and portability"),
    );
    expect(section.length, "README has no first-run section").toBeGreaterThan(400);

    // The generated identity: who, what, where, and the mode.
    expect(section).toContain("agentos up");
    expect(section).toContain("agentos onboard");
    expect(section).toContain("AGENTOS_API_KEY");
    expect(section).toContain("32-byte");
    expect(section).toContain("0600");
    expect(section.toLowerCase()).toContain("never overwritten");
    expect(section.toLowerCase()).toContain("print");

    // The line the review found missing: AgentOS must never fabricate a
    // provider credential, only its own bearer token.
    expect(section).toContain("Never");
    expect(section).toContain("provider credential");

    // doctor must name the cause, not "missing identities".
    expect(section).toContain("agentos doctor");
    expect(section).toContain("default route");
    expect(section).toContain("missing identities");
  });

  it("keeps the documented `agentos start` claim honest against the CLI", async () => {
    const readme = await Bun.file(new URL("README.md", repository)).text();
    const cli = await Bun.file(new URL("crates/cli/src/main.rs", repository)).text();
    const commands = cli.slice(cli.indexOf("enum Commands {"), cli.indexOf("\n}\n", cli.indexOf("enum Commands {")));
    const hasStart = /^ {4}Start\s*(?:\{|\(|,)/m.test(commands);

    if (hasStart) {
      expect(
        readme,
        "crates/cli still declares `start`, so README must state that it loads the same .env as `up`",
      ).toContain("`agentos start` | Loads the same active `.env` as `agentos up`.");
    } else {
      expect(
        readme.includes("agentos start"),
        "crates/cli no longer declares `start`, so README must stop documenting it",
      ).toBe(false);
    }
  });
});
