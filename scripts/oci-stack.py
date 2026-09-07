#!/usr/bin/env python3
"""Run the AgentOS runtime behind an OCI network boundary; never publish its raw bus."""
from __future__ import annotations

import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parent.parent
OWNER_LABEL = "io.unitb.agentos.owner"
ENGINE_LABEL = "io.unitb.agentos.engine"
CONTAINER_HOME = "/home/agentos/.agentos"
IMAGE = os.environ.get("AGENTOS_OCI_IMAGE", "localhost/unitb-agentos:local")


class StackError(Exception):
    pass


class CommandError(StackError):
    def __init__(self, phase: str, returncode: int, hint: str = ""):
        self.returncode = returncode
        super().__init__(f"OCI {phase} failed (exit {returncode}){hint}")


def safe_error(error: Exception) -> str:
    # Never render subprocess arguments, stderr, or provider output in a failure summary.
    if isinstance(error, StackError):
        return str(error)
    if isinstance(error, subprocess.TimeoutExpired):
        return f"command timed out after {error.timeout}s"
    if isinstance(error, OSError):
        return f"filesystem/process error {error.errno} ({os.strerror(error.errno) if error.errno else 'unknown'})"
    return type(error).__name__


def command(arguments: list[str], *, timeout: int = 30, capture: bool = True) -> str:
    phase = arguments[1]
    if phase in {"container", "image"}:
        phase += f" {arguments[2]}"
    try:
        result = subprocess.run(arguments, text=True, stdout=subprocess.PIPE if capture else None,
                                stderr=subprocess.PIPE if capture else None, timeout=timeout, check=False)
    except subprocess.TimeoutExpired:
        raise StackError(f"OCI {phase} timed out after {timeout}s") from None
    if result.returncode:
        diagnostic = (result.stderr or "").lower()
        hint = ""
        if "permission denied" in diagnostic:
            hint = "; check runtime/socket and file permissions"
        elif "already in use" in diagnostic or "name is already" in diagnostic:
            hint = "; container name conflict; no adoption/removal by name, inspect your runtime manually"
        elif "no such image" in diagnostic:
            hint = "; run build first and check AGENTOS_OCI_IMAGE"
        raise CommandError(phase, result.returncode, hint)
    return result.stdout.strip() if capture else ""


def runtime() -> str:
    configured = os.environ.get("AGENTOS_OCI_RUNTIME")
    candidates = [configured] if configured else ["podman", "docker"]
    failures = []
    for candidate in candidates:
        if not candidate or not shutil.which(candidate):
            continue
        try:
            info = command([candidate, "info", "--format", "{{json .}}"], timeout=15)
        except StackError as error:
            failures.append(str(error))
            if configured:
                raise StackError(f"Configured OCI runtime is unavailable: {error}; no fallback attempted") from None
            continue
        if Path(candidate).name == "docker":
            try:
                options = json.loads(info)["SecurityOptions"]
                if not isinstance(options, list) or not all(isinstance(option, str) for option in options):
                    raise ValueError
            except (ValueError, KeyError, TypeError):
                raise StackError("Cannot determine Docker rootless mode from info SecurityOptions; refusing startup") from None
            if any(option.lower() in {"rootless", "name=rootless"} for option in options):
                raise StackError("Rootless Docker is unsupported for the private bind mount with a non-root container UID; "
                                 "use rootless Podman with keep-id. Do not run the launcher as root")
        return candidate
    detail = f" Last check: {failures[-1]}" if failures else ""
    raise StackError("A running Podman or Docker runtime is required; native host startup is not supported." + detail)


def scope(home: Path) -> str:
    return hashlib.sha256(f"{os.getuid()}:{home}".encode()).hexdigest()[:24]


def prepare_home() -> Path:
    if os.getuid() == 0:
        raise StackError("Run AgentOS as a non-root user")
    raw = Path(os.environ.get("AGENTOS_OCI_HOME", str(Path.home() / ".agentos-oci"))).expanduser()
    if raw.is_symlink():
        raise StackError("AGENTOS_OCI_HOME must not be a symlink")
    home = raw.resolve()
    if any(c in str(home) for c in (",", "\n", "\r")):
        raise StackError("AGENTOS_OCI_HOME contains unsupported mount-path characters")
    home.mkdir(mode=0o700, parents=True, exist_ok=True)
    stat = home.stat()
    if stat.st_uid != os.getuid() or stat.st_mode & 0o077:
        raise StackError("AGENTOS_OCI_HOME must be owned by this user with mode 0700")
    return home


def preflight_pin(home: Path, engine: str) -> None:
    directory = home / "runtime"
    pin = directory / ".iii-version"
    if directory.is_symlink() or pin.is_symlink():
        raise StackError("Runtime directory and engine pin must not be symlinks")
    if not pin.exists():
        if directory.exists() and any(directory.iterdir()):
            raise StackError("Existing runtime has no engine pin; compatibility cannot be verified. "
                             "Preserve this home and select a separate AGENTOS_OCI_HOME for explicit migration")
        return
    current = pin.read_text().strip()
    if current != engine:
        def display(value):
            return value if re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.-]+)?", value) else "invalid pin"
        raise StackError(f"OCI home engine {display(current)} differs from checkout engine {display(engine)}. "
                         "Runtime files are unchanged; preserve this home and select a separate AGENTOS_OCI_HOME "
                         "for explicit migration. Engine homes are never automatically translated")


@contextlib.contextmanager
def operation_lock(home: Path):
    descriptor = os.open(home / "operation.lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise StackError("Another AgentOS operation owns this home") from None
        yield
    finally:
        os.close(descriptor)


def write_state(home: Path, value: dict) -> None:
    temporary = home / f"container.json.{os.getpid()}.tmp"
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(descriptor, "w") as stream:
            json.dump(value, stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, home / "container.json")
    finally:
        temporary.unlink(missing_ok=True)


def inspect(oci: str, identity: str, kind: str = "container") -> dict:
    try:
        values = json.loads(command([oci, kind, "inspect", identity]))
    except json.JSONDecodeError:
        raise StackError("OCI inspect returned invalid JSON") from None
    if not isinstance(values, list) or len(values) != 1 or not isinstance(values[0], dict):
        raise StackError("OCI inspect did not identify exactly one object")
    return values[0]


def immutable_id(value: str) -> str:
    if not isinstance(value, str):
        raise StackError("Invalid immutable OCI identity; refusing to act")
    identity = value.removeprefix("sha256:").lower()
    if not re.fullmatch(r"[0-9a-f]{64}", identity):
        raise StackError("Invalid immutable OCI identity; refusing to act")
    return identity


def verify_container(oci: str, home: Path, identity: str) -> dict:
    value = inspect(oci, identity)
    labels = value.get("Config", {}).get("Labels") or {}
    if immutable_id(value.get("Id", value.get("ID"))) != identity or labels.get(OWNER_LABEL) != scope(home):
        raise StackError("Container ownership mismatch; refusing to act")
    return {**value, "Id": identity}


def owned_container(oci: str, home: Path, *, prune_missing: bool = True) -> dict | None:
    path = home / "container.json"
    if path.is_symlink():
        raise StackError("Container ownership record must not be a symlink")
    if not path.exists():
        return None
    try:
        record = json.loads(path.read_text())
        identity = record["id"]
    except (ValueError, KeyError, TypeError):
        raise StackError("Invalid container ownership record; refusing to act") from None
    if not isinstance(identity, str) or not re.fullmatch(r"[0-9a-f]{64}", identity):
        raise StackError("Invalid container identity; refusing to act")
    try:
        return verify_container(oci, home, identity)
    except CommandError as error:
        # Only a successful immutable-ID listing proves absence. Daemon/permission failures
        # retain the record. Names and owner labels are never discovery/adoption authority.
        listed = command([oci, "container", "ls", "--all", "--no-trunc", "--filter", f"id={identity}",
                          "--format", "{{.ID}}"])
        identities = {immutable_id(item) for item in listed.splitlines() if item}
        if identity in identities:
            raise StackError(f"{error}; recorded container still exists, ownership record retained") from None
        if identities:
            raise StackError("OCI listing did not resolve the recorded identity; ownership record retained")
        if prune_missing:
            path.unlink()
            (home / "runtime.ready").unlink(missing_ok=True)
        return None


def launch_arguments(oci: str, home: Path, image: str, engine: str) -> list[str]:
    owner = scope(home)
    mount = f"type=bind,source={home},target={CONTAINER_HOME}"
    if Path(oci).name == "podman":
        mount += ",relabel=private"
    arguments = [oci, "run", "--detach", "--name", f"agentos-{owner}",
                 "--label", f"{OWNER_LABEL}={owner}", "--label", f"{ENGINE_LABEL}={engine}",
                 "--user", f"{os.getuid()}:{os.getgid()}", "--cap-drop=ALL",
                 "--security-opt=no-new-privileges", "--pids-limit=2048",
                 "--mount", mount,
                 "--publish", "127.0.0.1::3111", "--publish", "127.0.0.1::49134"]
    if Path(oci).name == "podman":
        arguments.extend(["--userns=keep-id"])
    return arguments + [image]


def status(value: dict, home: Path | None = None) -> dict:
    running = value.get("State", {}).get("Running", False)
    endpoints = {}
    if running:
        bindings = value.get("NetworkSettings", {}).get("Ports") or {}
        for name, port in (("api", "3111/tcp"), ("bus", "49134/tcp")):
            entries = bindings.get(port) or []
            if len(entries) != 1 or entries[0].get("HostIp") != "127.0.0.1":
                raise StackError("Container publish scope is not host-loopback-only")
            scheme = "http" if name == "api" else "ws"
            endpoints[name] = f"{scheme}://127.0.0.1:{int(entries[0]['HostPort'])}"
        unexpected = {p for p, entries in bindings.items() if entries} - {"3111/tcp", "49134/tcp"}
        if unexpected:
            raise StackError("Unexpected published runtime port")
    return {"container": value["Id"], "running": running,
            "ready": bool(running and home and (home / "runtime.ready").is_file()),
            "config_warning": bool(home and (home / "runtime.config-warning").is_file()),
            "engine": value["Config"]["Labels"][ENGINE_LABEL], "endpoints": endpoints}


def stop(oci: str, home: Path, value: dict) -> None:
    identity = value["Id"]
    if value.get("State", {}).get("Running"):
        # Entrypoint budgets: terminate 35s + kill/reap 5s + agentos stop 40s.
        command([oci, "stop", "--time", "90", identity], timeout=110)
    command([oci, "rm", identity])
    (home / "container.json").unlink(missing_ok=True)
    (home / "runtime.ready").unlink(missing_ok=True)


def save_logs(oci: str, home: Path, identity: str) -> Path:
    log = home / "last-boot.log"
    try:
        descriptor = os.open(log, os.O_CREAT | os.O_TRUNC | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
        with os.fdopen(descriptor, "w") as stream:
            os.fchmod(stream.fileno(), 0o600)
            try:
                result = subprocess.run([oci, "logs", identity], stdout=stream, stderr=subprocess.STDOUT,
                                        timeout=15, check=False)
            except subprocess.TimeoutExpired:
                raise StackError(f"Log capture timed out after 15s; partial private log: {log}") from None
            if result.returncode:
                raise StackError(f"Log capture failed (exit {result.returncode}); private log: {log}")
    except OSError as error:
        raise StackError(f"Private log capture failed at {log}: {safe_error(error)}") from None
    return log


def print_status(value: dict, home: Path) -> None:
    report = status(value, home)
    if report["config_warning"]:
        print("AgentOS: shipped config changed or its baseline is unknown; operator config was preserved. "
              "Review runtime/.config.shipped and reconcile config explicitly", file=sys.stderr)
    print(json.dumps(report, indent=2))


def start(oci: str, home: Path, engine: str) -> None:
    preflight_pin(home, engine)
    try:
        image = inspect(oci, IMAGE, "image")
    except StackError as error:
        raise StackError(f"Image preflight: {error}; run build first and check AGENTOS_OCI_IMAGE/runtime selection") from None
    if (image.get("Config", {}).get("Labels") or {}).get(ENGINE_LABEL) != engine:
        raise StackError("Image engine pin differs from this checkout; run build first")
    image_id = immutable_id(image.get("Id", image.get("ID")))
    existing = owned_container(oci, home)
    if existing:
        if immutable_id(existing.get("Image")) != image_id:
            raise StackError("Existing owned container belongs to another image; explicitly run stop, then up")
        if existing.get("State", {}).get("Running"):
            print_status(existing, home)
            return
        log = save_logs(oci, home, existing["Id"])
        stop(oci, home, existing)
        print(f"AgentOS: recovered stopped same-image owned container; previous private log: {log}", file=sys.stderr)
    # A SIGKILL can leave this marker behind. Clear it before the container can run/probe.
    (home / "runtime.ready").unlink(missing_ok=True)
    receipt = command(launch_arguments(oci, home, image_id, engine), timeout=60)
    try:
        identity = immutable_id(receipt)
    except StackError:
        raise StackError("Container creation returned no valid immutable ID; no cleanup by name/label was attempted") from None
    phase = "ownership record write"
    try:
        write_state(home, {"id": identity, "owner": scope(home)})
        deadline = time.monotonic() + 300
        last_probe = "not ready"
        while time.monotonic() < deadline:
            phase = "readiness inspection"
            value = owned_container(oci, home)
            if not value or not value.get("State", {}).get("Running"):
                exit_code = (value or {}).get("State", {}).get("ExitCode")
                detail = f"exit {exit_code}" if isinstance(exit_code, int) else "exit unknown"
                raise StackError(f"AgentOS container exited during startup ({detail})")
            phase = "readiness probe"
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                break
            try:
                probe = subprocess.run([oci, "exec", identity, "test", "-f", f"{CONTAINER_HOME}/runtime.ready"],
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                       timeout=min(10, remaining), check=False)
            except subprocess.TimeoutExpired:
                last_probe = "probe timeout"
            else:
                if probe.returncode == 0:
                    phase = "publish-scope validation"
                    print_status(value, home)
                    return
                last_probe = f"probe exit {probe.returncode}"
            time.sleep(1)
        raise StackError(f"AgentOS readiness deadline exceeded (300s; {last_probe})")
    except (StackError, OSError, subprocess.TimeoutExpired) as error:
        details = []
        try:
            # The immutable receipt from THIS run is cleanup authority even if persisting
            # its record failed. Still verify ID + home scope; never fall back to a name.
            value = verify_container(oci, home, identity)
        except (StackError, OSError, subprocess.TimeoutExpired) as verification:
            details.append(f"Cleanup refused: {safe_error(verification)}; immutable ID {identity} retained")
        else:
            try:
                log = save_logs(oci, home, identity)
                details.append(f"Private log: {log}")
            except (StackError, OSError, subprocess.TimeoutExpired) as logging:
                details.append(f"Private log capture at {home / 'last-boot.log'}: {safe_error(logging)}")
            try:
                stop(oci, home, value)
                details.append("Owned container stopped and removed")
            except (StackError, OSError, subprocess.TimeoutExpired) as cleanup:
                details.append(f"Cleanup failed: {safe_error(cleanup)}; immutable ID {identity} retained")
        raise StackError(f"Startup failed during {phase}: {safe_error(error)}. " + ". ".join(details)) from None


def main(arguments: list[str]) -> None:
    if not arguments or arguments[0] in ("--help", "-h", "help"):
        print("Usage: bash scripts/oci-stack.sh build|up|stop|status|logs|doctor|exec COMMAND...\n"
              "AGENTOS_OCI_HOME: private persistent home (default ~/.agentos-oci)\n"
              "AGENTOS_OCI_RUNTIME: explicit podman or docker executable")
        return
    action, *extra = arguments
    if action not in {"build", "up", "stop", "status", "logs", "doctor", "exec"} or (extra and action != "exec"):
        raise StackError("Unknown command or unexpected arguments; see --help")
    if action == "exec" and not extra:
        raise StackError("exec requires a command")
    oci = runtime()
    home = prepare_home()
    engine = (ROOT / ".iii-version").read_text().strip() if action in {"build", "up"} else ""
    if action == "up":
        preflight_pin(home, engine)  # Refuse another-engine home before even creating a lock file.
    mutating = action in {"build", "up", "stop"}
    with operation_lock(home) if mutating else contextlib.nullcontext():
        if action == "build":
            command([oci, "build", "--file", str(ROOT / "Containerfile"), "--tag", IMAGE,
                     "--label", f"{ENGINE_LABEL}={engine}", str(ROOT)], timeout=3600, capture=False)
        elif action == "up":
            start(oci, home, engine)
        else:
            value = owned_container(oci, home, prune_missing=mutating)
            if not value:
                if action in {"stop", "status"}:
                    print(json.dumps({"running": False, "ready": False, "owned_container": None}))
                    return
                raise StackError("No owned AgentOS container is running")
            if action == "stop":
                stop(oci, home, value)
            elif action == "status":
                print_status(value, home)
            elif action == "logs":
                command([oci, "logs", "--tail", "200", value["Id"]], capture=False)
            else:
                args = [oci, "exec", "--interactive"]
                if sys.stdin.isatty():
                    args.append("--tty")
                args.extend([value["Id"], *(extra if action == "exec" else ["agentos", "doctor"])])
                command(args, timeout=600, capture=False)


if __name__ == "__main__":
    try:
        main(sys.argv[1:])
    except (StackError, OSError, subprocess.TimeoutExpired) as error:
        print(f"AgentOS: {safe_error(error)}", file=sys.stderr)
        sys.exit(1)
