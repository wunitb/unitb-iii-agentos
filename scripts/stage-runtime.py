#!/usr/bin/env python3
"""Stage only explicit release inputs; never copy checkout credentials or runtime data."""
from __future__ import annotations

from pathlib import Path
import shutil
import sys
import tomllib


def stage(source: Path, destination: Path) -> None:
    if destination.exists() and any(destination.iterdir()):
        raise ValueError("Staging destination must be empty")
    runtime = destination / "runtime"
    binaries = runtime / "target/release"
    binaries.mkdir(parents=True)
    (destination / "bin").mkdir()
    for name in ("agentos", "agentos-tui", "agentos-bus-authd"):
        shutil.copy2(source / "target/release" / name, destination / "bin" / name)
    for name in (".iii-version", "config.yaml", "worker-compose.yaml", ".env.example"):
        shutil.copy2(source / name, runtime / name)
    for name in ("config", "agents", "hands", "identity", "integrations", "plugin", "workflows"):
        shutil.copytree(source / name, runtime / name)
    (runtime / "workers").mkdir()
    shutil.copy2(source / "workers/env.allowlist", runtime / "workers/env.allowlist")
    for manifest in sorted((source / "workers").glob("*/iii.worker.yaml")):
        worker = manifest.parent.name
        target = runtime / "workers" / worker
        target.mkdir()
        shutil.copy2(manifest, target / manifest.name)
        cargo = manifest.parent / "Cargo.toml"
        if cargo.is_file():
            package = tomllib.loads(cargo.read_text())["package"]["name"]
            shutil.copy2(source / "target/release" / package, binaries / f"agentos-{worker}")
        elif worker == "embedding":
            for name in ("main.py", "pyproject.toml", "uv.lock"):
                shutil.copy2(manifest.parent / name, target / name)
        else:
            raise ValueError(f"Unknown worker package: {worker}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: stage-runtime.py SOURCE EMPTY_DESTINATION")
    stage(Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve())
