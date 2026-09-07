#!/usr/bin/env python3
"""Initialize an owned persistent runtime and supervise its foreground lifecycle."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import threading

TEMPLATE = Path("/opt/agentos/runtime")
HOME = Path(os.environ["AGENTOS_HOME"])
RUNTIME = HOME / "runtime"
READY = HOME / "runtime.ready"
STOP = threading.Event()


def config_digest() -> str:
    digest = hashlib.sha256()
    inputs = [TEMPLATE / "config.yaml", *sorted((TEMPLATE / "config").rglob("*"))]
    for source in inputs:
        if source.is_file():
            digest.update(source.relative_to(TEMPLATE).as_posix().encode() + b"\0")
            digest.update(hashlib.sha256(source.read_bytes()).digest())
    return digest.hexdigest()


def write_private(path: Path, content: str) -> None:
    temporary = path.with_name(f"{path.name}.{os.getpid()}.tmp")
    descriptor = os.open(temporary, os.O_CREAT | os.O_EXCL | os.O_WRONLY | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(descriptor, "w") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def prepare() -> None:
    os.umask(0o077)
    if RUNTIME.is_symlink():
        raise RuntimeError("Runtime directory must not be a symlink")
    current_pin = RUNTIME / ".iii-version"
    if current_pin.is_symlink():
        raise RuntimeError("Runtime engine pin must not be a symlink")
    if current_pin.exists() and current_pin.read_text().strip() != (TEMPLATE / ".iii-version").read_text().strip():
        raise RuntimeError("Existing runtime uses another engine version; files are unchanged. "
                           "Preserve it and select a separate AGENTOS_OCI_HOME for explicit migration; no automatic translation")
    if not current_pin.exists() and RUNTIME.exists() and any(RUNTIME.iterdir()):
        raise RuntimeError("Existing runtime has no engine pin; preserve it and select a separate OCI home for explicit migration")
    shipped = config_digest()
    record = RUNTIME / ".config.shipped"
    seeded = None if any((RUNTIME / name).exists() for name in ("config.yaml", "config")) else shipped
    if record.is_symlink():
        raise RuntimeError("Shipped config record must not be a symlink")
    if record.exists():
        try:
            seeded = json.loads(record.read_text())["seeded_sha256"]
            if seeded is not None and (not isinstance(seeded, str) or len(seeded) != 64
                                       or any(c not in "0123456789abcdef" for c in seeded)):
                raise ValueError
        except (ValueError, KeyError, TypeError):
            raise RuntimeError("Invalid shipped config digest record; preserve it and reconcile config explicitly") from None
    HOME.mkdir(parents=True, exist_ok=True)
    RUNTIME.mkdir(exist_ok=True)
    READY.unlink(missing_ok=True)
    # All template inputs EXCEPT config.yaml/config are managed and refreshed each boot.
    # Operator .env and persistent data/state/skills/logs are never template inputs.
    for source in TEMPLATE.iterdir():
        destination = RUNTIME / source.name
        if destination.is_symlink():
            raise RuntimeError(f"Refusing symlink at managed runtime input {source.name}")
        if source.name in {"config", "config.yaml"} and destination.exists():
            continue
        if source.is_dir():
            shutil.copytree(source, destination, dirs_exist_ok=True)
        else:
            shutil.copy2(source, destination)
    write_private(record, json.dumps({"seeded_sha256": seeded, "shipped_sha256": shipped}) + "\n")
    warning = HOME / "runtime.config-warning"
    if seeded != shipped:
        message = ("Shipped config baseline is unknown" if seeded is None else "Shipped config template changed")
        message += "; operator config.yaml/config were preserved. Review runtime/.config.shipped and reconcile explicitly."
        write_private(warning, message + "\n")
        print(f"AgentOS: {message}", file=sys.stderr)
    else:
        warning.unlink(missing_ok=True)
    dotenv = RUNTIME / ".env"
    if not dotenv.exists():
        dotenv.touch(mode=0o600)
    os.environ["AGENTOS_CONFIG"] = str(RUNTIME / "config.yaml")
    os.environ["AGENTOS_CONTAINER_RUNTIME"] = "1"
    os.chdir(RUNTIME)


def main() -> int:
    signal.signal(signal.SIGTERM, lambda *_: STOP.set())
    signal.signal(signal.SIGINT, lambda *_: STOP.set())
    prepare()
    try:
        child = subprocess.Popen(["agentos", "up", "--no-tui"])
        while child.poll() is None:
            if STOP.wait(0.2):
                child.terminate()
                try:
                    child.wait(timeout=35)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
                return 1
        if child.returncode:
            return child.returncode
        READY.touch(mode=0o600)
        failures = 0
        while not STOP.wait(2):
            try:
                health = subprocess.run(["agentos", "status"], stdout=subprocess.DEVNULL,
                                        stderr=subprocess.DEVNULL, timeout=20, check=False)
                failure = f"exit {health.returncode}" if health.returncode else None
            except subprocess.TimeoutExpired:
                failure = "timeout after 20s"
            failures = failures + 1 if failure else 0
            if failure:
                print(f"AgentOS health probe: {failure} ({failures}/3 consecutive failures)", file=sys.stderr)
            if failures >= 3:
                return 1
        return 0
    finally:
        READY.unlink(missing_ok=True)
        subprocess.run(["agentos", "stop"], timeout=40, check=False)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"AgentOS container: {error}", file=sys.stderr)
        sys.exit(1)
