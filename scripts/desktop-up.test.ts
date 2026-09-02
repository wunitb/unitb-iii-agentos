import { execFile } from "node:child_process";
import { createServer, type Server } from "node:http";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { afterEach, describe, expect, it } from "vitest";

const execFileAsync = promisify(execFile);
const fixtureRoots: string[] = [];
const servers: Server[] = [];

interface DesktopResult {
  exitCode: number;
  stdout: string;
  stderr: string;
  /// One line per `iii` invocation, arguments joined by a space.
  iiiCalls: string[];
}

/// A console stand-in. `redirect` reproduces the standalone `iii-console`,
/// which answers `/` with a redirect to `/workers` and has no Chat route.
async function startConsole(mode: "ready" | "redirect" | "absent"): Promise<string> {
  if (mode === "absent") {
    // A port nothing listens on: the readiness poll must time out.
    return "http://127.0.0.1:1";
  }
  const server = createServer((request, response) => {
    if (mode === "redirect") {
      response.writeHead(302, { Location: "/workers" });
      response.end();
      return;
    }
    response.writeHead(200, { "Content-Type": "text/html" });
    response.end("<html>chat</html>");
  });
  servers.push(server);
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (address === null || typeof address === "string") throw new Error("no console port");
  return `http://127.0.0.1:${address.port}`;
}

async function runDesktopUp(
  mode: "ready" | "redirect" | "absent",
  options: { consoleEnabled?: boolean } = {},
): Promise<DesktopResult> {
  const root = await mkdtemp(join(tmpdir(), "agentos-desktop-up-"));
  fixtureRoots.push(root);

  // The engine config decides whether the console worker boots at all.
  const workers = ["  - name: state", ...(options.consoleEnabled === false ? [] : ["  - name: console"])];
  await writeFile(join(root, "config.yaml"), `workers:\n${workers.join("\n")}\n`);

  const scriptPath = join(root, "scripts", "desktop-up.sh");
  const stubDir = join(root, "stub");
  const callLog = join(root, "iii.calls");
  await mkdir(join(root, "scripts"), { recursive: true });
  await mkdir(stubDir, { recursive: true });
  await writeFile(
    scriptPath,
    await readFile(new URL("./desktop-up.sh", import.meta.url), "utf8"),
  );

  const iiiStub = join(stubDir, "iii");
  await writeFile(
    iiiStub,
    `#!/usr/bin/env bash\nprintf '%s\\n' "$*" >> ${JSON.stringify(callLog)}\nexit 0\n`,
  );
  await chmod(iiiStub, 0o755);

  const consoleUrl = await startConsole(mode);
  let exitCode = 0;
  let stdout = "";
  let stderr = "";
  try {
    const result = await execFileAsync("bash", [scriptPath], {
      encoding: "utf8",
      env: {
        PATH: `${stubDir}:${process.env.PATH}`,
        HOME: process.env.HOME,
        III_CONSOLE_URL: consoleUrl,
      },
      timeout: 90_000,
    });
    stdout = result.stdout;
    stderr = result.stderr;
  } catch (error) {
    const failure = error as { code?: number | string | null; stdout?: string; stderr?: string };
    if (typeof failure.code !== "number") throw error;
    exitCode = failure.code;
    stdout = failure.stdout ?? "";
    stderr = failure.stderr ?? "";
  }

  let iiiCalls: string[] = [];
  try {
    iiiCalls = (await readFile(callLog, "utf8")).split("\n").filter(Boolean);
  } catch {
    iiiCalls = [];
  }
  return { exitCode, stdout, stderr, iiiCalls };
}

afterEach(async () => {
  await Promise.all(
    servers.splice(0).map((server) => new Promise<void>((resolve) => server.close(() => resolve()))),
  );
  await Promise.all(fixtureRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("desktop-up console install", () => {
  it("installs the registry workers instead of only verifying the lockfile", async () => {
    const { exitCode, iiiCalls } = await runDesktopUp("ready");

    expect(exitCode).toBe(0);
    // `sync --frozen` verifies "without mutating local files" (iii 0.22.1
    // `worker sync --help`), so on a host without the console artifacts it can
    // never install anything and the readiness poll can only time out.
    expect(iiiCalls).toContain("worker sync");
    expect(iiiCalls.some((call) => call.includes("--frozen"))).toBe(false);
  });

  it("verifies config.yaml against iii.lock before installing", async () => {
    const { iiiCalls } = await runDesktopUp("ready");

    expect(iiiCalls[0]).toBe("worker verify --strict");
    expect(iiiCalls.indexOf("worker verify --strict")).toBeLessThan(
      iiiCalls.indexOf("worker sync"),
    );
  });

  it("reports the console as ready once it answers 200", async () => {
    const { exitCode, stdout } = await runDesktopUp("ready");

    expect(exitCode).toBe(0);
    expect(stdout).toContain("iii desktop chat console ready");
  });

  it("says so at once when the console worker is not enabled in config.yaml", async () => {
    const { exitCode, stderr, iiiCalls } = await runDesktopUp("absent", { consoleEnabled: false });

    // Without this the script installs artifacts and then polls a dead port for
    // 60 seconds before failing with no explanation.
    expect(exitCode).toBe(1);
    expect(stderr).toContain("console is not enabled");
    expect(stderr).toContain("0.0.0.0:3113");
    expect(stderr).toContain("- name: console");
    expect(iiiCalls).toEqual([]);
  });

  it("refuses to run when the standalone iii-console squats on the port", async () => {
    const { exitCode, stderr, iiiCalls } = await runDesktopUp("redirect");

    expect(exitCode).toBe(1);
    expect(stderr).toContain("standalone iii-console is occupying 3113");
    expect(iiiCalls).toEqual([]);
  });
});
