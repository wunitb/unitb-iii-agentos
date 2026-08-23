import { afterEach, describe, expect, it } from "bun:test";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const repository = new URL("../", import.meta.url);
const sandboxes: string[] = [];

async function executable(path: string, source: string): Promise<void> {
  await writeFile(path, source);
  await chmod(path, 0o755);
}

interface UpgradeFixturePaths {
  agentosHome: string;
  runtime: string;
}

async function upgradeFixture(
  beforeInstall?: (paths: UpgradeFixturePaths) => Promise<void>,
) {
  const root = await mkdtemp(join(tmpdir(), "agentos-install-upgrade-"));
  sandboxes.push(root);
  const home = join(root, "home");
  const agentosHome = join(home, ".agentos");
  const runtime = join(agentosHome, "runtime");
  const bin = join(home, ".local", "bin");
  const stubs = join(root, "stubs");
  const payload = join(root, "payload");
  const archive = join(root, "release.tar.gz");

  await Promise.all([
    mkdir(join(runtime, "config"), { recursive: true }),
    mkdir(join(runtime, "data", "sessions"), { recursive: true }),
    mkdir(bin, { recursive: true }),
    mkdir(stubs, { recursive: true }),
    mkdir(join(payload, "bin"), { recursive: true }),
    mkdir(join(payload, "runtime", "config"), { recursive: true }),
    mkdir(join(payload, "runtime", "data"), { recursive: true }),
    mkdir(join(payload, "runtime", "workers", "fresh"), { recursive: true }),
  ]);

  await Promise.all([
    writeFile(join(runtime, "config.yaml"), "operator: true\n"),
    writeFile(join(runtime, "config", "state.yaml"), "operator-state-config\n"),
    writeFile(join(runtime, "data", "state.db"), "sqlite-page-sentinel\u0000\u0001"),
    writeFile(join(runtime, "data", "sessions", "s-1.json"), '{"turns":7}\n'),
    writeFile(join(runtime, ".env"), "ANTHROPIC_API_KEY=preserve-me\n"),
    writeFile(join(runtime, "stale-release-file"), "remove me\n"),
    writeFile(join(payload, "runtime", "config.yaml"), "release: default\n"),
    writeFile(join(payload, "runtime", "config", "state.yaml"), "release default\n"),
    writeFile(join(payload, "runtime", "data", "default.db"), "release default\n"),
    writeFile(join(payload, "runtime", "workers", "fresh", "iii.worker.yaml"), "name: fresh\n"),
  ]);
  await executable(join(payload, "bin", "agentos"), "#!/bin/sh\nexit 0\n");
  await executable(join(stubs, "iii"), "#!/bin/sh\nprintf '0.22.1\\n'\n");
  await executable(
    join(stubs, "curl"),
    `#!/bin/sh
set -eu
out=''
url=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    -o|-fsSLo) shift; out="$1" ;;
    -*) ;;
    *) url="$1" ;;
  esac
  shift
done
case "$url" in
  *.sha256)
    name="\${url##*/}"
    hash="$(sha256sum "$AGENTOS_TEST_ARCHIVE" | cut -d ' ' -f 1)"
    printf '%s  %s\\n' "$hash" "\${name%.sha256}" > "$out"
    ;;
  *) cp "$AGENTOS_TEST_ARCHIVE" "$out" ;;
esac
`,
  );

  const packed = Bun.spawnSync({
    cmd: ["tar", "-czf", archive, "-C", payload, "."],
    stdout: "pipe",
    stderr: "pipe",
  });
  expect(packed.exitCode, packed.stderr.toString()).toBe(0);

  await beforeInstall?.({ agentosHome, runtime });

  const process = Bun.spawn(["bash", new URL("scripts/install.sh", repository).pathname], {
    env: {
      ...Bun.env,
      HOME: home,
      AGENTOS_HOME: agentosHome,
      BIN_DIR: bin,
      AGENTOS_VERSION: "v0.1.0",
      AGENTOS_TEST_ARCHIVE: archive,
      PATH: `${stubs}:${bin}:${Bun.env.PATH ?? ""}`,
      SHELL: "/bin/sh",
    },
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(process.stdout).text(),
    new Response(process.stderr).text(),
    process.exited,
  ]);
  expect(exitCode, `${stdout}\n${stderr}`).toBe(0);
  return { runtime };
}

afterEach(async () => {
  await Promise.all(
    sandboxes.splice(0).map((path) => rm(path, { recursive: true, force: true })),
  );
});

describe("installer upgrade portability", () => {
  it("preserves runtime data byte-for-byte while replacing release payload", async () => {
    const { runtime } = await upgradeFixture();

    expect(await readFile(join(runtime, "data", "state.db"))).toEqual(
      Buffer.from("sqlite-page-sentinel\u0000\u0001"),
    );
    expect(await readFile(join(runtime, "data", "sessions", "s-1.json"), "utf8")).toBe(
      '{"turns":7}\n',
    );
    expect(await Bun.file(join(runtime, "stale-release-file")).exists()).toBe(false);
    expect(await Bun.file(join(runtime, "workers", "fresh", "iii.worker.yaml")).exists()).toBe(
      true,
    );
  });

  it("preserves operator config and dotenv state during an upgrade", async () => {
    const { runtime } = await upgradeFixture();

    expect(await readFile(join(runtime, "config.yaml"), "utf8")).toBe("operator: true\n");
    expect(await readFile(join(runtime, "config", "state.yaml"), "utf8")).toBe(
      "operator-state-config\n",
    );
    expect(await readFile(join(runtime, ".env"), "utf8")).toBe(
      "ANTHROPIC_API_KEY=preserve-me\n",
    );
  });

  it("preserves empty operator-owned files and directories", async () => {
    const { runtime } = await upgradeFixture(async ({ runtime }) => {
      await Promise.all([
        writeFile(join(runtime, "config.yaml"), ""),
        writeFile(join(runtime, ".env"), ""),
        rm(join(runtime, "config"), { recursive: true, force: true }),
        rm(join(runtime, "data"), { recursive: true, force: true }),
      ]);
      await Promise.all([
        mkdir(join(runtime, "config"), { recursive: true }),
        mkdir(join(runtime, "data"), { recursive: true }),
      ]);
    });

    expect(await readFile(join(runtime, "config.yaml"), "utf8")).toBe("");
    expect(await readFile(join(runtime, ".env"), "utf8")).toBe("");
    expect(await readdir(join(runtime, "config"))).toEqual([]);
    expect(await readdir(join(runtime, "data"))).toEqual([]);
  });

  it("uses release defaults when no previous runtime exists", async () => {
    const { runtime } = await upgradeFixture(async ({ runtime }) => {
      await rm(runtime, { recursive: true, force: true });
    });

    expect(await readFile(join(runtime, "config.yaml"), "utf8")).toBe(
      "release: default\n",
    );
    expect(await readFile(join(runtime, "config", "state.yaml"), "utf8")).toBe(
      "release default\n",
    );
    expect(await readFile(join(runtime, "data", "default.db"), "utf8")).toBe(
      "release default\n",
    );
    expect(await Bun.file(join(runtime, ".env")).exists()).toBe(false);
  });

  it("recovers operator state from an interrupted retired runtime", async () => {
    const { runtime } = await upgradeFixture(async ({ agentosHome }) => {
      const retired = join(agentosHome, "runtime.old");
      await Promise.all([
        mkdir(join(retired, "config"), { recursive: true }),
        mkdir(join(retired, "data"), { recursive: true }),
      ]);
      await Promise.all([
        writeFile(join(retired, "config.yaml"), "retired-operator: true\n"),
        writeFile(join(retired, "config", "state.yaml"), "retired-state-config\n"),
        writeFile(join(retired, "data", "state.db"), "retired-live-state\n"),
        writeFile(join(retired, ".env"), "CODEX_PROXY_API_KEY=retired-secret\n"),
      ]);
    });

    expect(await readFile(join(runtime, "config.yaml"), "utf8")).toBe(
      "retired-operator: true\n",
    );
    expect(await readFile(join(runtime, "data", "state.db"), "utf8")).toBe(
      "retired-live-state\n",
    );
    expect(await readFile(join(runtime, ".env"), "utf8")).toBe(
      "CODEX_PROXY_API_KEY=retired-secret\n",
    );
    expect(await Bun.file(join(runtime, "..", "runtime.old")).exists()).toBe(false);
    expect(await Bun.file(join(runtime, "..", "runtime.new")).exists()).toBe(false);
  });

  it("keeps the published installer byte-identical to the source installer", async () => {
    expect(await readFile(new URL("scripts/install.sh", repository))).toEqual(
      await readFile(new URL("website/public/install.sh", repository)),
    );
  });
});
