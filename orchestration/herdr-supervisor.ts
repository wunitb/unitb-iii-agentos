#!/usr/bin/env bun

const session = process.env.HERDR_SESSION ?? "tldrsoc";
const herdr = process.env.HERDR_BIN ?? `${process.env.HOME}/.local/bin/herdr`;
let stopping = false;
let child: ReturnType<typeof Bun.spawn> | undefined;

async function healthy(): Promise<boolean> {
  const probe = Bun.spawn([herdr, "--session", session, "status"], { stdout: "ignore", stderr: "ignore" });
  return (await probe.exited) === 0;
}

async function supervise(): Promise<void> {
  while (!stopping) {
    if (await healthy()) {
      await Bun.sleep(2_000);
      continue;
    }
    child = Bun.spawn([herdr, "--session", session, "server"], {
      stdin: "ignore",
      stdout: "inherit",
      stderr: "inherit",
      env: { ...process.env, HERDR_SESSION: session },
    });
    await child.exited;
    child = undefined;
    if (!stopping) await Bun.sleep(1_000);
  }
}

function shutdown(): void {
  stopping = true;
  child?.kill("SIGTERM");
}
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
await supervise();

export {};
