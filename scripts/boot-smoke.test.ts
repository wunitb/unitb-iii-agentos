import { execFile } from "node:child_process";
import { chmod, copyFile, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { promisify } from "node:util";
import { afterEach, describe, expect, it } from "vitest";

const execFileAsync = promisify(execFile);
const fixtureRoots: string[] = [];

const requiredFunctionIds = [
  "agentos::llm::complete",
  "agentos::llm::route",
  "agent::chat",
  "memory::recall",
  "context::build_prompt",
  "cron::create",
].sort();

async function processExists(pid: number): Promise<boolean> {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ESRCH") return false;
    throw error;
  }
}

async function missingFunctionFixture(): Promise<{
  exitCode: number;
  stderr: string;
  tmpRoot: string;
  childPid: number;
  portProbe: string;
}> {
  const root = await mkdtemp(join(tmpdir(), "agentos-boot-smoke-test-"));
  fixtureRoots.push(root);
  const scripts = join(root, "scripts");
  const release = join(root, "target", "release");
  const stub = join(root, "stub");
  const tmpRoot = join(root, "tmp");
  const childPidFile = join(root, "child.pid");
  const portProbeFile = join(root, "port-probe.seen");
  await Promise.all([
    mkdir(scripts, { recursive: true }),
    mkdir(release, { recursive: true }),
    mkdir(stub, { recursive: true }),
    mkdir(tmpRoot, { recursive: true }),
  ]);
  await copyFile(new URL("./boot-smoke.sh", import.meta.url), join(scripts, "boot-smoke.sh"));
  await Promise.all([
    writeFile(join(root, "config.yaml"), "workers: []\n"),
    writeFile(join(root, "iii.lock"), "# fixture\n"),
    writeFile(join(root, ".iii-version"), "0.22.1\n"),
  ]);

  for (const name of ["agent-core", "llm-router", "memory", "context-manager", "cron", "other"]) {
    await mkdir(join(root, "workers", name), { recursive: true });
    await writeFile(
      join(root, "workers", name, "iii.worker.yaml"),
      `iii: v1\nname: ${name}\nruntime:\n  kind: rust\nscripts:\n  start: fixture\n`,
    );
    const worker = join(release, `agentos-${name}`);
    const body = name === "agent-core"
      ? "#!/bin/sh\nwhile :; do sleep 1; done\n"
      : "#!/bin/sh\nexit 0\n";
    await writeFile(worker, body);
    await chmod(worker, 0o755);
  }

  const agentos = join(release, "agentos");
  await writeFile(
    agentos,
    "#!/bin/sh\n" +
      "test \"$1 $2\" = \"up --no-tui\" || exit 64\n" +
      // Model the real iii-worker helper: it is reparented after this stub
      // exits, its argv contains no scratch path yet, and only its inherited
      // scratch HOME proves ownership before the delayed exec.
      "/bin/sh -c 'sleep 1; mkdir -p \"$HOME/.iii/workers\"; " +
      "cp /bin/sleep \"$HOME/.iii/workers/provider-late\"; " +
      "exec \"$HOME/.iii/workers/provider-late\" 60' &\n" +
      "printf '%s\\n' $! > \"$SMOKE_CHILD_PID_FILE\"\n",
  );
  await chmod(agentos, 0o755);

  const functions = [
    { function_id: "agentos::llm::route", worker_name: "agentos-llm-router" },
    { function_id: "agent::chat", worker_name: "agentos-agent-core" },
    { function_id: "memory::recall", worker_name: "agentos-memory" },
    { function_id: "context::build_prompt", worker_name: "agentos-context-manager" },
    { function_id: "cron::create", worker_name: "agentos-cron" },
  ];
  // Ensure every expected worker identity is represented even though the
  // function assertion must fail first on the deliberately absent id.
  functions.push({ function_id: "fixture::other", worker_name: "agentos-other" });
  const iii = join(stub, "iii");
  await writeFile(iii, `#!/bin/sh\nprintf '%s\\n' '${JSON.stringify({ functions })}'\n`);
  await chmod(iii, 0o755);
  const realPython = (await execFileAsync("/bin/sh", ["-c", "command -v python3"])).stdout.trim();
  const python = join(stub, "python3");
  await writeFile(
    python,
    "#!/bin/sh\n" +
      "if [ \"$#\" -eq 2 ] && [ \"$1\" = - ] && [ \"$2\" = 49134 ]; then " +
      "printf 'seen\n' > \"$SMOKE_PORT_PROBE_FILE\"; exit 1; fi\n" +
      `exec ${JSON.stringify(realPython)} \"$@\"\n`,
  );
  await chmod(python, 0o755);

  let exitCode = 0;
  let stderr = "";
  try {
    await execFileAsync("/bin/sh", [join(scripts, "boot-smoke.sh")], {
      env: {
        PATH: `${stub}:${process.env.PATH}`,
        TMPDIR: tmpRoot,
        SMOKE_CHILD_PID_FILE: childPidFile,
        SMOKE_PORT_PROBE_FILE: portProbeFile,
      },
      timeout: 15_000,
    });
  } catch (error) {
    const commandError = error as { code?: number | string | null; stderr?: string };
    if (typeof commandError.code !== "number") throw error;
    exitCode = commandError.code;
    stderr = commandError.stderr ?? "";
  }
  const childPid = Number((await readFile(childPidFile, "utf8")).trim());
  const portProbe = (await readFile(portProbeFile, "utf8")).trim();
  return { exitCode, stderr, tmpRoot, childPid, portProbe };
}

afterEach(async () => {
  await Promise.all(fixtureRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

describe("boot smoke contract", () => {
  it("checks the product entry points that make the AgentOS layer usable", async () => {
    const source = await readFile(new URL("./boot-smoke.sh", import.meta.url), "utf8");
    const declaration = source.match(/REQUIRED_FUNCTION_IDS='([\s\S]*?)'/);
    expect(declaration).not.toBeNull();
    const asserted = declaration?.[1].trim().split(/\s+/).sort();
    expect(asserted).toEqual(requiredFunctionIds);
    expect(source).toContain(
      'python3 - "$registry_file" "$expected_workers_file" "$required_functions_file"',
    );
  });

  it("names a missing function and reaps a late helper whose argv has no scratch path", async () => {
    const { exitCode, stderr, tmpRoot, childPid, portProbe } = await missingFunctionFixture();

    expect(exitCode).not.toBe(0);
    expect(stderr).toContain("missing function id(s): agentos::llm::complete");
    expect(portProbe).toBe("seen");
    expect(await readdir(tmpRoot)).toEqual([]);
    expect(await processExists(childPid)).toBe(false);
  });

  it("installs an exit trap that also refuses to leave the engine port occupied", async () => {
    const source = await readFile(new URL("./boot-smoke.sh", import.meta.url), "utf8");
    expect(source).toMatch(/trap cleanup (?:0|EXIT)/);
    expect(source).toMatch(/if port_is_open; then[\s\S]*status=1/);
    expect(source).toContain('export HOME="$engine_home"');
    expect(source).toContain('Path("/proc")');
    expect(source).toMatch(/quiet_observations=.*0[\s\S]*quiet_observations.*-lt 3/);
    expect(source).toContain('rm -rf "$scratch"');
  });

  it("runs as a bounded required job after release artifacts exist", async () => {
    const workflow = await readFile(new URL("../.github/workflows/ci.yml", import.meta.url), "utf8");
    const job = workflow.match(/\n  boot-smoke:\n([\s\S]*?)(?=\n  [a-z][a-z-]+:\n|$)/)?.[1];
    expect(job).toBeDefined();
    expect(job).toMatch(/needs: rust/);
    expect(job).toMatch(/timeout-minutes: \d+/);
    expect(job).toContain("name: worker-binaries");
    expect(job).toContain("bash scripts/boot-smoke.sh");
  });
});
