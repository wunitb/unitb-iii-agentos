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

const deniedFunctionIds = [
  "agentos::bus_auth",
  "mcp::list_connections",
  "bridge::list",
  "configuration::set",
  "agent::chat",
  "integration::add",
  "integration::remove",
  "pulse::register",
  "pulse::invoke",
  "pulse::status",
  "pulse::toggle",
  "swarm::create",
  "swarm::broadcast",
  "swarm::dissolve",
  "a2a::handle_task",
];

type DenialMode = "rbac" | "generic" | "success";

async function processExists(pid: number): Promise<boolean> {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ESRCH") return false;
    throw error;
  }
}

async function runSmokeFixture(options: {
  denialMode?: DenialMode;
  completeRegistry?: boolean;
  requireBuiltinDisableUnset?: boolean;
} = {}): Promise<{
  exitCode: number;
  stderr: string;
  tmpRoot: string;
  childPid: number;
  portProbes: string[];
}> {
  const denialMode = options.denialMode ?? "rbac";
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
  const builtinEnvCheck = options.requireBuiltinDisableUnset
    ? 'test -z "${IIIWORKER_DISABLE_BUILTIN_DAEMONS:-}" || { echo inherited-builtin-disable >&2; exit 65; }\n'
    : "";
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
      "printf '%s\\n' $! > \"$SMOKE_CHILD_PID_FILE\"\n" +
      builtinEnvCheck,
  );
  await chmod(agentos, 0o755);

  const functions = [
    { function_id: "agentos::llm::route", worker_name: "agentos-llm-router" },
    { function_id: "agent::chat", worker_name: "agentos-agent-core" },
    { function_id: "memory::recall", worker_name: "agentos-memory" },
    { function_id: "context::build_prompt", worker_name: "agentos-context-manager" },
    { function_id: "cron::create", worker_name: "agentos-cron" },
  ];
  if (options.completeRegistry) {
    functions.push({
      function_id: "agentos::llm::complete",
      worker_name: "agentos-llm-router",
    });
  }
  // Ensure every expected worker identity is represented even though the
  // function assertion must fail first on the deliberately absent id.
  functions.push({ function_id: "fixture::other", worker_name: "agentos-other" });
  const iii = join(stub, "iii");
  const deniedCase = deniedFunctionIds.join("|");
  const deniedResponse = denialMode === "rbac"
    ? String.raw`printf '%s\n' "Error: {\"code\":\"FORBIDDEN\",\"message\":\"function '$2' not allowed (remove from rbac.forbidden_functions)\"}" >&2; exit 1`
    : denialMode === "generic"
    ? `printf '%s\n' "function '$2' not allowed by fixture transport" >&2; exit 1`
    : `printf '%s\n' '{"ok":true}'; exit 0`;
  await writeFile(
    iii,
    `#!/bin/sh
if [ "$1" = trigger ] && [ "$2" = engine::functions::list ]; then
  printf '%s\n' '${JSON.stringify({ functions })}'
  exit 0
fi
case "$2" in
  ${deniedCase}) ${deniedResponse} ;;
esac
printf '%s\n' "unexpected fixture call: $*" >&2
exit 65
`,
  );
  await chmod(iii, 0o755);
  const realPython = (await execFileAsync("/bin/sh", ["-c", "command -v python3"])).stdout.trim();
  const python = join(stub, "python3");
  await writeFile(
    python,
    "#!/bin/sh\n" +
      "if [ \"$#\" -eq 2 ] && [ \"$1\" = - ]; then " +
      "case \"$2\" in 49129|49134) printf '%s\\n' \"$2\" >> " +
      "\"$SMOKE_PORT_PROBE_FILE\"; exit 1;; esac; fi\n" +
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
        ...(options.requireBuiltinDisableUnset
          ? { IIIWORKER_DISABLE_BUILTIN_DAEMONS: "inherited-test-value" }
          : {}),
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
  const portProbes = (await readFile(portProbeFile, "utf8")).trim().split(/\s+/);
  return { exitCode, stderr, tmpRoot, childPid, portProbes };
}

afterEach(async () => {
  await Promise.all(fixtureRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

async function expectFixtureReaped(result: Awaited<ReturnType<typeof runSmokeFixture>>) {
  expect(await readdir(result.tmpRoot)).toEqual([]);
  expect(await processExists(result.childPid)).toBe(false);
}

describe("boot smoke contract", () => {
  it("checks the product entry points that make the AgentOS layer usable", async () => {
    const source = await readFile(new URL("./boot-smoke.sh", import.meta.url), "utf8");
    const declaration = source.match(/REQUIRED_FUNCTION_IDS='([\s\S]*?)'/);
    expect(declaration).not.toBeNull();
    const asserted = declaration?.[1].trim().split(/\s+/).sort();
    expect(asserted).toEqual(requiredFunctionIds);
    const deniedDeclaration = source.match(/UNTRUSTED_DENIED_FUNCTION_IDS='([\s\S]*?)'/);
    expect(deniedDeclaration).not.toBeNull();
    expect(deniedDeclaration?.[1].trim().split(/\s+/).sort()).toEqual(
      [...deniedFunctionIds].sort(),
    );
    expect(source).toContain(
      'python3 - "$registry_file" "$expected_workers_file" "$required_functions_file"',
    );
  });

  it("names a missing function after accepting exact engine RBAC denials", async () => {
    const { exitCode, stderr, tmpRoot, childPid, portProbes } = await runSmokeFixture();

    expect(exitCode).not.toBe(0);
    expect(stderr).toContain("missing function id(s): agentos::llm::complete");
    expect(portProbes).toContain("49129");
    expect(portProbes).toContain("49134");
    expect(await readdir(tmpRoot)).toEqual([]);
    expect(await processExists(childPid)).toBe(false);
  });

  it("accepts exact FORBIDDEN responses and a complete registry", async () => {
    const result = await runSmokeFixture({ completeRegistry: true });
    expect(result.exitCode).toBe(0);
    expect(result.stderr).not.toContain("reason other than the exact engine RBAC deny");
    await expectFixtureReaped(result);
  });

  it("rejects a generic command failure even when it says not allowed", async () => {
    const result = await runSmokeFixture({ denialMode: "generic", completeRegistry: true });
    expect(result.exitCode).not.toBe(0);
    expect(result.stderr).toContain("reason other than the exact engine RBAC deny");
    await expectFixtureReaped(result);
  });

  it("rejects an unexpected success from a supposedly denied function", async () => {
    const result = await runSmokeFixture({ denialMode: "success", completeRegistry: true });
    expect(result.exitCode).not.toBe(0);
    expect(result.stderr).toContain("untrusted call unexpectedly reached agentos::bus_auth");
    await expectFixtureReaped(result);
  });

  it("clears a parent builtin-daemon override before invoking agentos", async () => {
    const result = await runSmokeFixture({
      completeRegistry: true,
      requireBuiltinDisableUnset: true,
    });
    expect(result.exitCode).toBe(0);
    expect(result.stderr).not.toContain("inherited-builtin-disable");
    await expectFixtureReaped(result);
  });

  it("requires the engine's exact RBAC remediation marker", async () => {
    const source = await readFile(new URL("./boot-smoke.sh", import.meta.url), "utf8");
    expect(source).toContain("remove from rbac.forbidden_functions");
  });

  it("installs an exit trap that refuses to leave either boundary port occupied", async () => {
    const source = await readFile(new URL("./boot-smoke.sh", import.meta.url), "utf8");
    expect(source).toMatch(/trap cleanup (?:0|EXIT)/);
    expect(source).toContain('BUS_AUTH_PORT=49129');
    expect(source).toMatch(/for port in "\$BUS_AUTH_PORT" "\$ENGINE_PORT"/);
    expect(source).toMatch(/if port_is_open "\$port"; then[\s\S]*status=1/);
    expect(source).toContain('unset IIIWORKER_DISABLE_BUILTIN_DAEMONS');
    expect(source).not.toContain('export IIIWORKER_DISABLE_BUILTIN_DAEMONS=1');
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
