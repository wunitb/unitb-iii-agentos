import { describe, expect, it } from "bun:test";

const repository = new URL("../../", import.meta.url);

describe("README bootstrap quickstart", () => {
  it("documents build, up, no-TUI, and doctor as the supported flow", async () => {
    const readme = await Bun.file(new URL("README.md", repository)).text();

    expect(readme).toContain("bash scripts/oci-stack.sh build");
    expect(readme).toContain("bash scripts/oci-stack.sh up");
    expect(readme).toContain("`up` is headless");
    expect(readme).toContain("bash scripts/oci-stack.sh doctor");
    expect(readme).toContain("The TUI opens on Chat");
  });

  it("documents the first-run key contract that `up` and `onboard` implement", async () => {
    const readme = await Bun.file(new URL("README.md", repository)).text();
    const section = readme.slice(
      readme.indexOf("## § 03 · Quickstart"),
      readme.indexOf("### Archived native v0.2.0"),
    );
    expect(section.length, "README has no first-run section").toBeGreaterThan(400);

    // The generated identity: who, what, where, and the mode.
    expect(section).toContain("scripts/oci-stack.sh up");
    expect(section).toContain("agentos onboard");
    expect(section).toContain("AGENTOS_API_KEY");
    expect(section).toContain("32-byte");
    expect(section).toContain("0600");
    expect(section.toLowerCase()).toContain("never overwritten");
    expect(section.toLowerCase()).toContain("print");

    // The line the review found missing: AgentOS must never fabricate a
    // provider credential, only its own bearer token.
    expect(section).toContain("Never");
    expect(section.toLowerCase()).toContain("provider credential");

    // doctor must name the cause, not "missing identities".
    expect(section).toContain("agentos doctor");
    expect(section).toContain("default route");
    expect(section).toContain("missing identities");
  });

  it("documents an in-place write, because appending would break the documented next command", async () => {
    const readme = await Bun.file(new URL("README.md", repository)).text();
    const example = await Bun.file(new URL(".env.example", repository)).text();
    const bootstrap = await Bun.file(new URL("crates/cli/src/bootstrap.rs", repository)).text();

    // The two facts that make "append" wrong, asserted against the tree rather
    // than trusted: the template already declares the name, and the startup
    // script refuses a file that assigns one name twice.
    expect(/^AGENTOS_API_KEY=\s*$/m.test(example), ".env.example no longer ships an empty AGENTOS_API_KEY=").toBe(true);
    expect(bootstrap).toContain("Duplicate dotenv variable");

    expect(readme, "README must say the key is written in place, not appended").toContain("in place");
    expect(readme).toContain("Duplicate dotenv variable");
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
    if (table === undefined) throw new Error("llm-router preference table is missing");

    const order = [...table.matchAll(/"([a-z-]+)"|\b([A-Z_]+_PROVIDER)\b/g)].map((match) =>
      match[1] ?? (match[2] === "CODEX_PROVIDER" ? "codex" : match[2]!),
    );
    expect(order.length).toBeGreaterThan(1);
    expect(router).toContain("provider_credential_missing");

    // README must list exactly that order, in that order.
    const start = readme.indexOf("The default provider");
    const end = readme.indexOf("Explicit provider/model selection", start);
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
      expect(readme).toContain("`agentos up`/`agentos start`");
      expect(readme).toContain("must not be used with this OCI-only config");
      const bootstrap = await Bun.file(new URL("crates/cli/src/bootstrap.rs", repository)).text();
      expect(bootstrap).toContain("require_container");
    } else {
      expect(
        readme.includes("agentos start"),
        "crates/cli no longer declares `start`, so README must stop documenting it",
      ).toBe(false);
    }
  });
});
