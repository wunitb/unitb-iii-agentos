#!/usr/bin/env python3
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ContainerRuntimeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "home"
        self.template = self.root / "template"
        self.template.mkdir()
        for name, content in {".iii-version": "0.23.0\n", "config.yaml": "new-default",
                              "worker-compose.yaml": "new-compose"}.items():
            (self.template / name).write_text(content)
        (self.template / "config").mkdir()
        (self.template / "config" / "agent.yaml").write_text("new-agent")
        environment = patch.dict(os.environ, {"AGENTOS_HOME": str(self.home)})
        environment.start()
        self.addCleanup(environment.stop)
        self.entry = load("container_entrypoint", "container-entrypoint.py")
        self.entry.TEMPLATE = self.template
        self.addCleanup(os.chdir, Path.cwd())
        old_umask = os.umask(0o077)
        self.addCleanup(os.umask, old_umask)

    def test_fresh_home_has_private_credentials_and_complete_runtime(self):
        self.entry.prepare()
        self.assertEqual((self.entry.RUNTIME / ".env").stat().st_mode & 0o777, 0o600)
        self.assertTrue((self.entry.RUNTIME / "worker-compose.yaml").is_file())
        self.assertEqual(os.environ["AGENTOS_CONFIG"], str(self.entry.RUNTIME / "config.yaml"))
        self.assertEqual(Path.cwd(), self.entry.RUNTIME)
        self.assertFalse(self.entry.READY.exists())

    def test_restart_keeps_operator_config_credentials_and_data(self):
        self.entry.prepare()
        files = {"config.yaml": "operator-config", "config/agent.yaml": "operator-agent",
                 ".env": "fixture-only", "data/record": "retained-data"}
        before = {}
        for name, text in files.items():
            path = self.entry.RUNTIME / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
            before[name] = path.stat().st_ino
        self.entry.READY.touch()
        self.entry.prepare()
        for name, text in files.items():
            path = self.entry.RUNTIME / name
            self.assertEqual(path.read_text(), text)
            self.assertEqual(path.stat().st_ino, before[name])
        self.assertFalse(self.entry.READY.exists())

    def test_another_engine_home_is_not_translated(self):
        self.entry.RUNTIME.mkdir(parents=True)
        pin = self.entry.RUNTIME / ".iii-version"
        pin.write_text("0.22.1\n")
        with self.assertRaisesRegex(RuntimeError, "another engine version"):
            self.entry.prepare()
        self.assertEqual(pin.read_text(), "0.22.1\n")
        self.assertFalse((self.entry.RUNTIME / ".env").exists())

    def test_symlink_runtime_is_refused_without_writing_target(self):
        self.home.mkdir()
        outside = self.root / "outside"
        outside.mkdir()
        self.entry.RUNTIME.symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(RuntimeError, "symlink"):
            self.entry.prepare()
        self.assertEqual(list(outside.iterdir()), [])


    def test_seeded_config_digest_is_private_and_operator_changes_are_not_template_drift(self):
        self.entry.prepare()
        path = self.entry.RUNTIME / ".config.shipped"
        record = json.loads(path.read_text())
        self.assertEqual(record["seeded_sha256"], self.entry.config_digest())
        self.assertEqual(record["seeded_sha256"], record["shipped_sha256"])
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        (self.entry.RUNTIME / "config.yaml").write_text("operator changes")
        with contextlib.redirect_stderr(io.StringIO()) as output:
            self.entry.prepare()
        self.assertEqual(output.getvalue(), "")
        self.assertFalse((self.home / "runtime.config-warning").exists())

    def test_same_engine_template_drift_warns_repeatedly_and_preserves_operator_config(self):
        self.entry.prepare()
        record = self.entry.RUNTIME / ".config.shipped"
        seeded = json.loads(record.read_text())["seeded_sha256"]
        operator = self.entry.RUNTIME / "config.yaml"
        operator.write_text("operator-private")
        inode = operator.stat().st_ino
        (self.template / "config/agent.yaml").write_text("changed-topology")
        for _ in range(2):
            with contextlib.redirect_stderr(io.StringIO()) as output:
                self.entry.prepare()
            self.assertIn("Shipped config template changed", output.getvalue())
            self.assertNotIn("operator-private", output.getvalue())
            current = json.loads(record.read_text())
            self.assertEqual(current["seeded_sha256"], seeded)
            self.assertNotEqual(current["shipped_sha256"], seeded)
            self.assertEqual(current["shipped_sha256"], self.entry.config_digest())
            self.assertEqual(operator.read_text(), "operator-private")
            self.assertEqual(operator.stat().st_ino, inode)
            self.assertEqual((self.entry.RUNTIME / "config/agent.yaml").read_text(), "new-agent")
            self.assertEqual((self.home / "runtime.config-warning").stat().st_mode & 0o777, 0o600)

    def test_legacy_config_without_digest_is_not_claimed_as_seeded(self):
        self.entry.RUNTIME.mkdir(parents=True)
        (self.entry.RUNTIME / ".iii-version").write_text("0.23.0\n")
        (self.entry.RUNTIME / "config.yaml").write_text("operator-config")
        with contextlib.redirect_stderr(io.StringIO()) as output:
            self.entry.prepare()
        record = json.loads((self.entry.RUNTIME / ".config.shipped").read_text())
        self.assertIsNone(record["seeded_sha256"])
        self.assertEqual(record["shipped_sha256"], self.entry.config_digest())
        self.assertIn("baseline is unknown", output.getvalue())
        self.assertEqual((self.entry.RUNTIME / "config.yaml").read_text(), "operator-config")

    def test_config_digest_includes_names_and_both_root_and_nested_content(self):
        original = self.entry.config_digest()
        nested = self.template / "config/agent.yaml"
        nested.rename(nested.with_name("renamed.yaml"))
        renamed = self.entry.config_digest()
        self.assertNotEqual(original, renamed)
        (self.template / "config.yaml").write_text("changed-root")
        self.assertNotEqual(renamed, self.entry.config_digest())

    def test_missing_engine_pin_and_invalid_digest_refuse_before_runtime_copy(self):
        self.entry.RUNTIME.mkdir(parents=True)
        config = self.entry.RUNTIME / "config.yaml"
        config.write_text("operator-config")
        with self.assertRaisesRegex(RuntimeError, "no engine pin"):
            self.entry.prepare()
        self.assertFalse((self.entry.RUNTIME / ".iii-version").exists())
        (self.entry.RUNTIME / ".iii-version").write_text("0.23.0\n")
        (self.entry.RUNTIME / ".config.shipped").write_text("provider-SECRET")
        with self.assertRaisesRegex(RuntimeError, "Invalid shipped config digest record") as caught:
            self.entry.prepare()
        self.assertNotIn("SECRET", str(caught.exception))
        self.assertFalse((self.entry.RUNTIME / "worker-compose.yaml").exists())
        self.assertEqual(config.read_text(), "operator-config")

    def run_health(self, outcomes):
        self.home.mkdir()
        child = Mock()
        child.poll.return_value = 0
        child.returncode = 0
        health = iter(outcomes)
        waits = iter([False] * len(outcomes) + [True])
        def run(args, **kwargs):
            if args == ["agentos", "status"]:
                self.assertEqual(kwargs["timeout"], 20)
                result = next(health)
                if result == "timeout":
                    raise subprocess.TimeoutExpired(["provider-SECRET"], 20)
                return subprocess.CompletedProcess(args, result)
            self.assertEqual(args, ["agentos", "stop"])
            self.assertEqual(kwargs["timeout"], 40)
            return subprocess.CompletedProcess(args, 0)
        with patch.object(self.entry, "prepare"), patch.object(self.entry.signal, "signal"), \
                patch.object(self.entry.subprocess, "Popen", return_value=child) as start, \
                patch.object(self.entry.subprocess, "run", side_effect=run) as command, \
                patch.object(self.entry.STOP, "wait", side_effect=lambda _seconds: next(waits)), \
                contextlib.redirect_stderr(io.StringIO()) as output:
            result = self.entry.main()
        self.startup_args = start.call_args.args[0]
        self.assertFalse(self.entry.READY.exists())
        self.assertEqual(command.call_args.args[0], ["agentos", "stop"])
        self.assertNotIn("SECRET", output.getvalue())
        return result, output.getvalue(), command

    def test_container_startup_budget_covers_compose_and_fits_outer_deadline(self):
        self.run_health([0])
        self.assertIn("--no-tui", self.startup_args)
        self.assertIn("--timeout", self.startup_args)
        budget = int(self.startup_args[self.startup_args.index("--timeout") + 1])
        self.assertGreaterEqual(budget, 4 * 60)  # Four dependent Compose startup layers.
        self.assertLess(budget, 300)  # Launcher retains its whole-startup deadline.

    def test_health_requires_three_consecutive_bounded_failures(self):
        result, output, command = self.run_health([1, "timeout", 1])
        self.assertEqual(result, 1)
        self.assertIn("timeout after 20s (2/3", output)
        self.assertIn("3/3 consecutive failures", output)
        self.assertEqual(command.call_count, 4)

    def test_health_success_resets_failures_and_timeout_does_not_exit_immediately(self):
        result, output, command = self.run_health([1, "timeout", 0, "timeout", 1, 0])
        self.assertEqual(result, 0)
        self.assertNotIn("3/3", output)
        self.assertEqual(output.count("1/3"), 2)
        self.assertEqual(command.call_count, 7)

    def test_health_three_timeouts_terminate_without_echoing_subprocess_arguments(self):
        result, output, command = self.run_health(["timeout"] * 3)
        self.assertEqual(result, 1)
        self.assertEqual(output.count("timeout after 20s"), 3)
        self.assertEqual(command.call_count, 4)

    def test_shutdown_inner_budgets_fit_launcher_grace(self):
        self.home.mkdir()
        child = Mock()
        child.poll.return_value = None
        child.wait.side_effect = [subprocess.TimeoutExpired(["agentos", "up"], 35), 0]
        with patch.object(self.entry, "prepare"), patch.object(self.entry.signal, "signal"), \
                patch.object(self.entry.subprocess, "Popen", return_value=child), \
                patch.object(self.entry.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as command, \
                patch.object(self.entry.STOP, "wait", return_value=True):
            self.assertEqual(self.entry.main(), 1)
        self.assertEqual([call.kwargs["timeout"] for call in child.wait.call_args_list], [35, 5])
        child.terminate.assert_called_once()
        child.kill.assert_called_once()
        self.assertEqual(command.call_args.kwargs["timeout"], 40)
        stack = load("oci_stack_budget", "oci-stack.py")
        with patch.object(stack, "command") as outer:
            stack.stop("podman", self.home, {"Id": "a" * 64, "State": {"Running": True}})
        grace = int(outer.call_args_list[0].args[0][3])
        self.assertGreater(grace, 35 + 5 + 40)
        self.assertGreater(outer.call_args_list[0].kwargs["timeout"], grace)


class RuntimeStagingTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.source = self.root / "source"
        self.destination = self.root / "bundle"
        self.source.mkdir()
        self.stage = load("stage_runtime", "stage-runtime.py").stage
        release = self.source / "target/release"
        release.mkdir(parents=True)
        for binary in ("agentos", "agentos-tui", "agentos-bus-authd", "agentos-demo"):
            (release / binary).write_text("fixture-binary")
        for name in (".iii-version", "config.yaml", "worker-compose.yaml", ".env.example"):
            (self.source / name).write_text("fixture-input")
        for name in ("config", "agents", "hands", "identity", "integrations", "plugin", "workflows"):
            (self.source / name).mkdir()
        workers = self.source / "workers"
        (workers / "demo").mkdir(parents=True)
        (workers / "embedding").mkdir()
        (workers / "env.allowlist").write_text("FIXTURE")
        (workers / "demo/iii.worker.yaml").write_text("name: demo")
        (workers / "demo/Cargo.toml").write_text('[package]\nname = "agentos-demo"\n')
        for name in ("iii.worker.yaml", "main.py", "pyproject.toml", "uv.lock"):
            (workers / "embedding" / name).write_text("fixture-worker")

    def test_explicit_bundle_has_compose_and_workers_without_private_runtime(self):
        (self.source / ".env").write_text("private-fixture")
        (self.source / "data").mkdir()
        (self.source / "data/operator").write_text("private-fixture")
        self.stage(self.source, self.destination)
        runtime = self.destination / "runtime"
        self.assertTrue((runtime / "worker-compose.yaml").is_file())
        self.assertTrue((runtime / "target/release/agentos-demo").is_file())
        self.assertTrue((runtime / "workers/embedding/main.py").is_file())
        self.assertTrue((self.destination / "bin/agentos-bus-authd").is_file())
        self.assertFalse((runtime / ".env").exists())
        self.assertFalse((runtime / "data").exists())
        self.assertFalse(any(p.is_symlink() for p in self.destination.rglob("*")))

    def test_nonempty_destination_is_not_overwritten(self):
        self.destination.mkdir()
        marker = self.destination / "keep"
        marker.write_text("operator-owned")
        with self.assertRaisesRegex(ValueError, "empty"):
            self.stage(self.source, self.destination)
        self.assertEqual(marker.read_text(), "operator-owned")


    def test_runtime_base_meets_registry_glibc_floor_without_changing_builder(self):
        # Registry iii-directory 1.2.6/aarch64 needs GLIBC_2.39. Trixie supplies 2.41;
        # bookworm's 2.36 remains suitable for our Rust builder, not that runtime worker.
        containerfile = Path(__file__).resolve().parent.parent / "Containerfile"
        bases = [line for line in containerfile.read_text().splitlines() if line.startswith("FROM ")]
        self.assertEqual(bases, ["FROM docker.io/library/rust:1.90-bookworm AS build",
                                 "FROM docker.io/library/debian:trixie-slim"])

    def test_image_normalizes_only_immutable_template_permissions(self):
        source = (Path(__file__).resolve().parent.parent / "Containerfile").read_text()
        self.assertIn("RUN chmod -R a+rX /opt/agentos/runtime", source)
        self.assertIn("COPY --chmod=0644 scripts/container-entrypoint.py", source)
        self.assertLess(source.index("RUN chmod -R a+rX /opt/agentos/runtime"),
                        source.index("USER agentos"))
        self.assertNotIn("chmod -R a+rX /home/", source)


if __name__ == "__main__":
    unittest.main()
