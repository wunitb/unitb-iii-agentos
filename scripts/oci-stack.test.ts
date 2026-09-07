import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const scripts = dirname(fileURLToPath(import.meta.url));

describe("OCI runtime boundary and ownership", () => {
  it("passes the isolated launcher ownership and publish-scope tests", () => {
    const result = spawnSync("python3", [join(scripts, "oci-stack.test.py")], {
      encoding: "utf8",
      timeout: 30_000,
    });
    expect(result.error).toBeUndefined();
    expect(result.status, result.stderr).toBe(0);
    expect(result.stderr).toContain("OK");
  });

  it("shows help without starting a container or creating a runtime home", () => {
    const result = spawnSync("bash", [join(scripts, "oci-stack.sh"), "--help"], {
      encoding: "utf8",
      timeout: 10_000,
      env: { ...process.env, AGENTOS_OCI_RUNTIME: "/no-runtime-for-help" },
    });
    expect(result.status, result.stderr).toBe(0);
    expect(result.stdout).toContain("AGENTOS_OCI_HOME");
  });

  it("rejects unknown commands before invoking the configured OCI runtime", () => {
    const result = spawnSync("bash", [join(scripts, "oci-stack.sh"), "remove-everything"], {
      encoding: "utf8",
      timeout: 10_000,
      env: { ...process.env, AGENTOS_OCI_RUNTIME: "/no-runtime-for-invalid-commands" },
    });
    expect(result.status).toBe(1);
    expect(result.stderr).toContain("Unknown command");
  });
});

describe("OCI bundle and persistent runtime", () => {
  it("stages explicit inputs and preserves operator state across startup", () => {
    const result = spawnSync("python3", [join(scripts, "container-runtime.test.py")], {
      encoding: "utf8",
      timeout: 30_000,
    });
    expect(result.error).toBeUndefined();
    expect(result.status, result.stderr).toBe(0);
    expect(result.stderr).toContain("OK");
  });
});

