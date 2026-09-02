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

  it("documents an in-place write, because appending would break the documented next command", async () => {
    const readme = await Bun.file(new URL("README.md", repository)).text();
    const example = await Bun.file(new URL(".env.example", repository)).text();
    const devUp = await Bun.file(new URL("scripts/dev-up.sh", repository)).text();

    // The two facts that make "append" wrong, asserted against the tree rather
    // than trusted: the template already declares the name, and the startup
    // script refuses a file that assigns one name twice.
    expect(/^AGENTOS_API_KEY=\s*$/m.test(example), ".env.example no longer ships an empty AGENTOS_API_KEY=").toBe(true);
    expect(devUp).toContain("duplicate dotenv variable");

    expect(readme, "README must say the key is written in place, not appended").toContain("in place");
    expect(readme).toContain("duplicate dotenv variable");
    expect(readme).not.toContain("append it to that `.env`");
  });

  it("describes automatic routing the way llm-router implements it", async () => {
    const readme = await Bun.file(new URL("README.md", repository)).text();

    expect(
      readme,
      "the stale claim that Anthropic is only chosen explicitly or as a local-default fallback",
    ).not.toContain("Anthropic is optional and selected only by");
    expect(readme).toContain("provider_credential_missing");

    const router = await Bun.file(new URL("workers/llm-router/src/main.rs", repository)).text();
    const table = /const AUTO_ROUTE_PREFERENCE: &\[&str\] = &\[([\s\S]*?)\];/.exec(router)?.[1];
    if (table === undefined) return; // preference table not landed yet

    const order = [...table.matchAll(/"([a-z-]+)"|([A-Z_]+_PROVIDER)/g)].map((match) =>
      match[1] ?? (match[2] === "CODEX_PROVIDER" ? "codex" : match[2]!),
    );
    expect(order.length).toBeGreaterThan(1);
    expect(router).toContain("provider_credential_missing");

    // README must list exactly that order, in that order.
    const start = readme.indexOf("walks a fixed");
    const end = readme.indexOf("Naming a provider");
    expect(start, "README no longer describes the preference order").toBeGreaterThan(-1);
    expect(end).toBeGreaterThan(start);
    const published = readme.slice(start, end);
    const positions = order.map((provider) => published.indexOf(`\`${provider}\``));
    for (const [index, position] of positions.entries()) {
      expect(position, `README does not list provider ${order[index]} in the preference order`).toBeGreaterThan(-1);
    }
    expect([...positions].sort((a, b) => a - b), "README lists the preference order out of order").toEqual(positions);
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
      ).toContain("`agentos start` | Loads the same active `.env` as `agentos up`");
    } else {
      expect(
        readme.includes("agentos start"),
        "crates/cli no longer declares `start`, so README must stop documenting it",
      ).toBe(false);
    }
  });
});
