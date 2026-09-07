#!/usr/bin/env python3
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("oci_stack", Path(__file__).with_name("oci-stack.py"))
STACK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(STACK)


class OciStackTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.home = Path(self.temporary.name)
        self.addCleanup(self.temporary.cleanup)

    def value(self):
        return {"Id": "a" * 64, "State": {"Running": True},
                "Config": {"Labels": {STACK.ENGINE_LABEL: "0.23.0", STACK.OWNER_LABEL: STACK.scope(self.home)}},
                "NetworkSettings": {"Ports": {"3111/tcp": [{"HostIp": "127.0.0.1", "HostPort": "12001"}],
                                                "49134/tcp": [{"HostIp": "127.0.0.1", "HostPort": "12002"}]}}}

    def test_only_api_and_gated_bus_are_published_to_host_loopback(self):
        args = STACK.launch_arguments("podman", self.home, "sha256:fixture", "0.23.0")
        publishes = [args[i + 1] for i, value in enumerate(args) if value == "--publish"]
        self.assertEqual(publishes, ["127.0.0.1::3111", "127.0.0.1::49134"])
        self.assertIn("--cap-drop=ALL", args)
        self.assertIn("--security-opt=no-new-privileges", args)
        self.assertIn("--userns=keep-id", args)
        self.assertNotIn("--privileged", args)
        self.assertNotIn("--network=host", args)
        self.assertEqual(args[-1], "sha256:fixture")
        self.assertNotIn("docker.sock", " ".join(args))

    def test_docker_does_not_receive_podman_only_options(self):
        args = STACK.launch_arguments("docker", self.home, "image", "0.23.0")
        self.assertNotIn("--userns=keep-id", args)
        self.assertEqual(args[args.index("--user") + 1], f"{os.getuid()}:{os.getgid()}")

    def test_scope_binds_user_and_home(self):
        self.assertNotEqual(STACK.scope(self.home), STACK.scope(self.home / "other"))
        with patch.object(STACK.os, "getuid", return_value=os.getuid() + 1):
            other_user = STACK.scope(self.home)
        self.assertNotEqual(STACK.scope(self.home), other_user)

    def test_state_is_private_and_atomic(self):
        STACK.write_state(self.home, {"id": "a" * 64})
        path = self.home / "container.json"
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(json.loads(path.read_text())["id"], "a" * 64)
        self.assertEqual(list(self.home.glob("*.tmp")), [])

    def test_foreign_container_is_never_adopted(self):
        STACK.write_state(self.home, {"id": "a" * 64})
        value = self.value()
        value["Config"]["Labels"][STACK.OWNER_LABEL] = "another-home"
        with patch.object(STACK, "inspect", return_value=value):
            with self.assertRaisesRegex(STACK.StackError, "ownership mismatch"):
                STACK.owned_container("podman", self.home)

    def test_bad_identity_is_rejected_before_inspection(self):
        STACK.write_state(self.home, {"id": "--all"})
        with patch.object(STACK, "inspect") as inspect:
            with self.assertRaises(STACK.StackError):
                STACK.owned_container("podman", self.home)
            inspect.assert_not_called()

    def test_symlink_record_is_not_followed(self):
        outside = self.home / "outside.json"
        outside.write_text("private")
        (self.home / "container.json").symlink_to(outside)
        with self.assertRaisesRegex(STACK.StackError, "symlink"):
            STACK.owned_container("podman", self.home)
        self.assertEqual(outside.read_text(), "private")

    def test_no_record_does_not_discover_or_adopt_other_containers(self):
        with patch.object(STACK, "inspect") as inspect:
            self.assertIsNone(STACK.owned_container("podman", self.home))
            inspect.assert_not_called()

    def test_status_has_no_inspect_environment_or_secrets(self):
        value = self.value()
        value["Config"]["Env"] = ["SECRET=not-for-output"]
        report = STACK.status(value)
        self.assertEqual(report["endpoints"]["api"], "http://127.0.0.1:12001")
        self.assertNotIn("not-for-output", json.dumps(report))

    def test_status_rejects_public_bind_and_extra_port(self):
        value = self.value()
        value["NetworkSettings"]["Ports"]["3111/tcp"][0]["HostIp"] = "0.0.0.0"
        with self.assertRaises(STACK.StackError):
            STACK.status(value)
        value = self.value()
        value["NetworkSettings"]["Ports"]["49129/tcp"] = [{"HostIp": "127.0.0.1", "HostPort": "12003"}]
        with self.assertRaises(STACK.StackError):
            STACK.status(value)

    def test_stopped_container_status_does_not_invent_endpoints(self):
        value = self.value()
        value["State"]["Running"] = False
        value["NetworkSettings"]["Ports"] = {}
        self.assertEqual(STACK.status(value)["endpoints"], {})

    def test_stop_uses_only_verified_immutable_id_and_preserves_data(self):
        STACK.write_state(self.home, {"id": "a" * 64})
        data = self.home / "operator-data"
        data.write_text("retain")
        with patch.object(STACK, "command") as command:
            STACK.stop("podman", self.home, self.value())
        self.assertEqual(command.call_args_list[0].args[0], ["podman", "stop", "--time", "90", "a" * 64])
        self.assertEqual(command.call_args_list[0].kwargs["timeout"], 110)
        self.assertEqual(command.call_args_list[1].args[0], ["podman", "rm", "a" * 64])
        self.assertEqual(data.read_text(), "retain")
        self.assertFalse((self.home / "container.json").exists())

    def test_invalid_action_fails_before_runtime_or_home_mutation(self):
        with patch.object(STACK, "runtime") as runtime:
            with self.assertRaises(STACK.StackError):
                STACK.main(["unknown"])
            runtime.assert_not_called()

    def test_image_override_is_read_without_running_container_commands(self):
        with patch.dict(os.environ, {"AGENTOS_OCI_IMAGE": "localhost/agentos-fixture:isolated"}):
            module = importlib.util.module_from_spec(SPEC)
            SPEC.loader.exec_module(module)
        self.assertEqual(module.IMAGE, "localhost/agentos-fixture:isolated")


    def image(self):
        return {"Id": "sha256:" + "b" * 64, "Config": {"Labels": {STACK.ENGINE_LABEL: "0.23.0"}}}

    def inspected(self, value):
        return lambda _oci, _identity, kind="container": self.image() if kind == "image" else value

    def test_podman_bind_uses_private_selinux_relabel_only(self):
        for oci in ("podman", "docker"):
            args = STACK.launch_arguments(oci, self.home, "image", "0.23.0")
            self.assertEqual("relabel=private" in args[args.index("--mount") + 1], oci == "podman")

    def test_root_refusal_precedes_home_creation(self):
        home = self.home / "must-not-exist"
        with patch.dict(os.environ, {"AGENTOS_OCI_HOME": str(home)}), patch.object(STACK.os, "getuid", return_value=0):
            with self.assertRaisesRegex(STACK.StackError, "non-root"):
                STACK.prepare_home()
        self.assertFalse(home.exists())

    def test_rootless_docker_diagnostic_does_not_suggest_root_bypass(self):
        info = json.dumps({"SecurityOptions": ["name=seccomp,profile=builtin", "name=rootless"]})
        with patch.dict(os.environ, {"AGENTOS_OCI_RUNTIME": "docker"}), \
                patch.object(STACK.shutil, "which", return_value="/usr/bin/docker"), \
                patch.object(STACK, "command", return_value=info) as command:
            with self.assertRaisesRegex(STACK.StackError, "Rootless Docker.*rootless Podman.*Do not run.*as root"):
                STACK.runtime()
        self.assertEqual(command.call_args.args[0], ["docker", "info", "--format", "{{json .}}"])

    def test_docker_info_must_identify_security_mode(self):
        with patch.dict(os.environ, {"AGENTOS_OCI_RUNTIME": "docker"}), \
                patch.object(STACK.shutil, "which", return_value="docker"), \
                patch.object(STACK, "command", return_value="{}"):
            with self.assertRaisesRegex(STACK.StackError, "Cannot determine Docker rootless mode"):
                STACK.runtime()

    def test_runtime_falls_back_only_when_unconfigured(self):
        with patch.dict(os.environ, {}, clear=True), patch.object(STACK.shutil, "which", return_value="runtime"), \
                patch.object(STACK, "command", side_effect=[STACK.CommandError("info", 125), '{"SecurityOptions": []}']):
            self.assertEqual(STACK.runtime(), "docker")
        with patch.dict(os.environ, {"AGENTOS_OCI_RUNTIME": "podman"}), \
                patch.object(STACK.shutil, "which", return_value="podman"), \
                patch.object(STACK, "command", side_effect=STACK.CommandError("info", 125)) as command:
            with self.assertRaisesRegex(STACK.StackError, "exit 125.*no fallback"):
                STACK.runtime()
            self.assertEqual(command.call_count, 1)

    def test_command_errors_expose_only_phase_exit_and_safe_hint(self):
        result = subprocess.CompletedProcess([], 125, "provider-SECRET", "permission denied provider-SECRET")
        with patch.object(STACK.subprocess, "run", return_value=result):
            with self.assertRaisesRegex(STACK.StackError, "container inspect.*exit 125.*permissions") as caught:
                STACK.command(["podman", "container", "inspect", "a" * 64])
        self.assertNotIn("SECRET", str(caught.exception))
        with patch.object(STACK.subprocess, "run", side_effect=subprocess.TimeoutExpired(["exec", "SECRET"], 10)):
            with self.assertRaisesRegex(STACK.StackError, "OCI exec timed out after 10s") as caught:
                STACK.command(["podman", "exec", "SECRET"], timeout=10)
        self.assertNotIn("SECRET", str(caught.exception))

    def test_pin_mismatch_preflight_does_not_inspect_launch_or_touch_old_home(self):
        directory = self.home / "runtime"
        directory.mkdir()
        (directory / ".iii-version").write_text("0.22.1\n")
        (directory / "data").write_text("retain")
        (self.home / "runtime.ready").touch()
        before = {p: (p.read_bytes(), p.stat().st_mtime_ns) for p in self.home.rglob("*") if p.is_file()}
        with patch.object(STACK, "inspect") as inspect, patch.object(STACK, "command") as command:
            with self.assertRaisesRegex(STACK.StackError, "0.22.1.*0.23.0.*separate AGENTOS_OCI_HOME"):
                STACK.start("podman", self.home, "0.23.0")
            inspect.assert_not_called()
            command.assert_not_called()
        self.assertEqual(before, {p: (p.read_bytes(), p.stat().st_mtime_ns) for p in self.home.rglob("*") if p.is_file()})
        checkout = self.home / "checkout"
        checkout.mkdir()
        (checkout / ".iii-version").write_text("0.23.0\n")
        with patch.object(STACK, "ROOT", checkout), patch.object(STACK, "runtime", return_value="podman"), \
                patch.object(STACK, "prepare_home", return_value=self.home), patch.object(STACK, "operation_lock") as lock:
            with self.assertRaises(STACK.StackError):
                STACK.main(["up"])
            lock.assert_not_called()

    def test_missing_home_pin_is_not_blindly_seeded_and_bad_pin_is_not_echoed(self):
        directory = self.home / "runtime"
        directory.mkdir()
        (directory / "config.yaml").write_text("operator config")
        with self.assertRaisesRegex(STACK.StackError, "no engine pin"):
            STACK.preflight_pin(self.home, "0.23.0")
        (directory / ".iii-version").write_text("provider-SECRET\n")
        with self.assertRaisesRegex(STACK.StackError, "invalid pin") as caught:
            STACK.preflight_pin(self.home, "0.23.0")
        self.assertNotIn("SECRET", str(caught.exception))

    def test_missing_image_preflight_has_build_hint(self):
        with patch.object(STACK, "inspect", side_effect=STACK.CommandError("image inspect", 125)):
            with self.assertRaisesRegex(STACK.StackError, "Image preflight.*exit 125.*run build first"):
                STACK.start("podman", self.home, "0.23.0")

    def test_missing_recorded_container_is_pruned_only_after_successful_id_listing(self):
        STACK.write_state(self.home, {"id": "a" * 64})
        (self.home / "runtime.ready").touch()
        with patch.object(STACK, "inspect", side_effect=STACK.CommandError("container inspect", 125)), \
                patch.object(STACK, "command", return_value="") as command:
            self.assertIsNone(STACK.owned_container("podman", self.home))
        self.assertEqual(command.call_args.args[0], ["podman", "container", "ls", "--all", "--no-trunc", "--filter",
                                                    "id=" + "a" * 64, "--format", "{{.ID}}"])
        self.assertFalse((self.home / "container.json").exists())
        self.assertFalse((self.home / "runtime.ready").exists())

    def test_unavailable_or_ambiguous_inspect_never_discards_record(self):
        STACK.write_state(self.home, {"id": "a" * 64})
        for listed in ("a" * 64, "unparseable", "c" * 64):
            with self.subTest(listed=listed), \
                    patch.object(STACK, "inspect", side_effect=STACK.CommandError("container inspect", 125)), \
                    patch.object(STACK, "command", return_value=listed):
                with self.assertRaises(STACK.StackError):
                    STACK.owned_container("podman", self.home)
                self.assertTrue((self.home / "container.json").exists())
        with patch.object(STACK, "inspect", side_effect=STACK.CommandError("container inspect", 125)), \
                patch.object(STACK, "command", side_effect=STACK.CommandError("container ls", 125)):
            with self.assertRaisesRegex(STACK.StackError, "container ls.*125"):
                STACK.owned_container("podman", self.home)
        self.assertTrue((self.home / "container.json").exists())

    def test_read_only_missing_check_leaves_state_and_marker_untouched(self):
        STACK.write_state(self.home, {"id": "a" * 64})
        (self.home / "runtime.ready").touch()
        with patch.object(STACK, "inspect", side_effect=STACK.CommandError("container inspect", 125)), \
                patch.object(STACK, "command", return_value=""):
            self.assertIsNone(STACK.owned_container("docker", self.home, prune_missing=False))
        self.assertTrue((self.home / "container.json").exists())
        self.assertTrue((self.home / "runtime.ready").exists())

    def test_stopped_same_image_recovers_only_after_private_logs_and_clears_stale_ready(self):
        STACK.write_state(self.home, {"id": "a" * 64})
        (self.home / "runtime.ready").touch()
        data = self.home / "operator-data"
        data.write_text("retained")
        old = self.value()
        old.update(Image="b" * 64, State={"Running": False})
        fresh = self.value()
        fresh.update(Id="c" * 64, Image="b" * 64)
        events = []
        def inspect(_oci, identity, kind="container"):
            return self.image() if kind == "image" else (old if identity == "a" * 64 else fresh)
        def command(args, **_kwargs):
            events.append(args[1])
            if args[1] == "run":
                self.assertFalse((self.home / "runtime.ready").exists())
                return "c" * 64
            self.assertEqual(args, ["podman", "rm", "a" * 64])
            return ""
        def logs(*_args):
            events.append("logs")
            return self.home / "last-boot.log"
        with patch.object(STACK, "inspect", side_effect=inspect), patch.object(STACK, "command", side_effect=command), \
                patch.object(STACK, "save_logs", side_effect=logs), \
                patch.object(STACK.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)), \
                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            STACK.start("podman", self.home, "0.23.0")
        self.assertEqual(events, ["logs", "rm", "run"])
        self.assertEqual(json.loads((self.home / "container.json").read_text())["id"], "c" * 64)
        self.assertEqual(data.read_text(), "retained")

    def test_different_image_never_automatically_stops_or_replaces_container(self):
        STACK.write_state(self.home, {"id": "a" * 64})
        for running in (True, False):
            value = self.value()
            value.update(Image="c" * 64, State={"Running": running})
            with self.subTest(running=running), patch.object(STACK, "inspect", side_effect=self.inspected(value)), \
                    patch.object(STACK, "command") as command, patch.object(STACK, "save_logs") as logs:
                with self.assertRaisesRegex(STACK.StackError, "another image.*explicitly run stop, then up"):
                    STACK.start("podman", self.home, "0.23.0")
                command.assert_not_called()
                logs.assert_not_called()

    def test_stopped_container_is_retained_if_saving_logs_fails(self):
        STACK.write_state(self.home, {"id": "a" * 64})
        value = self.value()
        value.update(Image="b" * 64, State={"Running": False})
        with patch.object(STACK, "inspect", side_effect=self.inspected(value)), \
                patch.object(STACK, "command") as command, \
                patch.object(STACK, "save_logs", side_effect=STACK.StackError("Log capture failed (exit 125)")):
            with self.assertRaisesRegex(STACK.StackError, "Log capture failed"):
                STACK.start("podman", self.home, "0.23.0")
            command.assert_not_called()
        self.assertTrue((self.home / "container.json").exists())

    def test_startup_exit_saves_private_logs_and_cleans_up_without_echoing_secrets(self):
        value = self.value()
        value.update(Image="b" * 64, State={"Running": False, "ExitCode": 47})
        log = self.home / "last-boot.log"
        log.touch(mode=0o644)
        def capture(args, **kwargs):
            self.assertEqual(args, ["podman", "logs", "a" * 64])
            kwargs["stdout"].write("provider-SECRET\n")
            return subprocess.CompletedProcess(args, 0)
        with patch.object(STACK, "inspect", side_effect=self.inspected(value)), \
                patch.object(STACK, "command", side_effect=lambda args, **_kw: "a" * 64 if args[1] == "run" else "") as command, \
                patch.object(STACK.subprocess, "run", side_effect=capture):
            with self.assertRaisesRegex(STACK.StackError, "readiness inspection.*exit 47.*Private log:.*stopped and removed") as caught:
                STACK.start("podman", self.home, "0.23.0")
        self.assertNotIn("SECRET", str(caught.exception))
        self.assertEqual(log.read_text(), "provider-SECRET\n")
        self.assertEqual(log.stat().st_mode & 0o777, 0o600)
        self.assertEqual(command.call_args_list[-1].args[0], ["podman", "rm", "a" * 64])
        self.assertFalse((self.home / "container.json").exists())

    def test_state_write_failure_after_creation_cleans_up_by_verified_receipt_even_if_logging_fails(self):
        value = self.value()
        value["Image"] = "b" * 64
        with patch.object(STACK, "inspect", side_effect=self.inspected(value)), \
                patch.object(STACK, "command", side_effect=lambda args, **_kw: "a" * 64 if args[1] == "run" else "") as command, \
                patch.object(STACK, "write_state", side_effect=OSError(28, "provider-SECRET")), \
                patch.object(STACK, "save_logs", side_effect=OSError(13, "provider-SECRET")):
            with self.assertRaisesRegex(STACK.StackError, "ownership record write.*28.*log capture.*13.*stopped and removed") as caught:
                STACK.start("podman", self.home, "0.23.0")
        self.assertNotIn("SECRET", str(caught.exception))
        self.assertEqual(command.call_args_list[-2].args[0], ["podman", "stop", "--time", "90", "a" * 64])
        self.assertEqual(command.call_args_list[-1].args[0], ["podman", "rm", "a" * 64])
        self.assertFalse((self.home / "container.json").exists())

    def test_state_write_failure_never_cleans_up_mismatched_owner(self):
        value = self.value()
        value["Config"]["Labels"][STACK.OWNER_LABEL] = "foreign"
        with patch.object(STACK, "inspect", side_effect=self.inspected(value)), \
                patch.object(STACK, "command", return_value="a" * 64) as command, \
                patch.object(STACK, "write_state", side_effect=OSError(28, "disk full")), \
                patch.object(STACK, "save_logs") as logs:
            with self.assertRaisesRegex(STACK.StackError, "Cleanup refused.*ownership mismatch"):
                STACK.start("podman", self.home, "0.23.0")
        self.assertEqual(command.call_count, 1)
        logs.assert_not_called()

    def test_readiness_timeout_is_retried_within_deadline(self):
        value = self.value()
        probes = 0
        def probe(_args, **kwargs):
            nonlocal probes
            self.assertLessEqual(kwargs["timeout"], 10)
            probes += 1
            if probes == 1:
                raise subprocess.TimeoutExpired(["provider-SECRET"], 10)
            (self.home / "runtime.ready").touch()
            return subprocess.CompletedProcess([], 0)
        with patch.object(STACK, "inspect", side_effect=self.inspected(value)), \
                patch.object(STACK, "command", return_value="a" * 64) as command, \
                patch.object(STACK.subprocess, "run", side_effect=probe), patch.object(STACK.time, "sleep"), \
                contextlib.redirect_stdout(io.StringIO()) as output:
            STACK.start("podman", self.home, "0.23.0")
        self.assertEqual(probes, 2)
        self.assertEqual(command.call_count, 1)
        self.assertTrue(json.loads(output.getvalue())["ready"])

    def test_readiness_deadline_cleans_up_and_retains_precise_phase(self):
        value = self.value()
        with patch.object(STACK, "inspect", side_effect=self.inspected(value)), \
                patch.object(STACK, "command", return_value="a" * 64) as command, \
                patch.object(STACK.time, "monotonic", side_effect=[0, 1, 2, 301]), patch.object(STACK.time, "sleep"), \
                patch.object(STACK.subprocess, "run", return_value=subprocess.CompletedProcess([], 1)), \
                patch.object(STACK, "save_logs", return_value=self.home / "last-boot.log"):
            with self.assertRaisesRegex(STACK.StackError, "readiness probe.*deadline exceeded.*300s.*probe exit 1"):
                STACK.start("podman", self.home, "0.23.0")
        self.assertEqual(command.call_args.args[0], ["podman", "rm", "a" * 64])

    def test_image_and_container_ids_are_normalized_without_adopting_by_name(self):
        for oci in ("docker", "podman"):
            with self.subTest(oci=oci):
                STACK.write_state(self.home, {"id": "a" * 64})
                value = self.value()
                value["Image"] = "sha256:" + "b" * 64 if oci == "docker" else "b" * 64
                if oci == "podman":
                    value["ID"] = value.pop("Id")
                with patch.object(STACK, "inspect", side_effect=self.inspected(value)), \
                        patch.object(STACK, "command") as command, contextlib.redirect_stdout(io.StringIO()):
                    STACK.start(oci, self.home, "0.23.0")
                command.assert_not_called()
        for invalid in ("agentos-name", "sha256:short", "a" * 63, None):
            with self.assertRaises(STACK.StackError):
                STACK.immutable_id(invalid)

    def test_status_logs_and_exec_are_unlocked_and_not_checkout_pin_bound(self):
        for action in (["status"], ["logs"], ["exec", "true"]):
            with self.subTest(action=action), patch.object(STACK, "runtime", return_value="podman"), \
                    patch.object(STACK, "prepare_home", return_value=self.home), patch.object(STACK, "ROOT", self.home), \
                    patch.object(STACK, "operation_lock") as lock, patch.object(STACK, "command"), \
                    patch.object(STACK, "owned_container", return_value=self.value()) as owned, \
                    contextlib.redirect_stdout(io.StringIO()):
                STACK.main(action)
                lock.assert_not_called()
                owned.assert_called_once_with("podman", self.home, prune_missing=False)

    def test_status_exposes_ready_and_sanitized_config_warning_only(self):
        value = self.value()
        self.assertFalse(STACK.status(value, self.home)["ready"])
        (self.home / "runtime.ready").touch()
        (self.home / "runtime.config-warning").write_text("provider-SECRET")
        with contextlib.redirect_stdout(io.StringIO()) as output, contextlib.redirect_stderr(io.StringIO()) as errors:
            STACK.print_status(value, self.home)
        self.assertTrue(json.loads(output.getvalue())["ready"])
        self.assertTrue(json.loads(output.getvalue())["config_warning"])
        self.assertIn("operator config was preserved", errors.getvalue())
        self.assertNotIn("SECRET", output.getvalue() + errors.getvalue())
        value["State"]["Running"] = False
        self.assertFalse(STACK.status(value, self.home)["ready"])

    def test_integrated_config_bind_and_port_contract_matches_launcher(self):
        checkout = Path(os.environ.get("AGENTOS_OCI_CONTRACT_ROOT", str(STACK.ROOT)))
        if (checkout / ".iii-version").read_text().strip() == "0.22.1":
            self.skipTest("isolated OCI worktree awaits principal's engine/runtime config integration")
        self.assertTrue((checkout / "worker-compose.yaml").is_file())
        config = (checkout / "config.yaml").read_text()
        blocks = dict(block.split("\n", 1) for block in re.split(r"(?m)^  - name: ", config)[1:])
        def scalar(block, key, indent):
            match = re.search(rf"(?m)^{' ' * indent}{key}: ([0-9.]+)\s*$", block)
            self.assertIsNotNone(match, f"missing explicit {key} in runtime config")
            return match.group(1)
        raw = blocks["iii-worker-manager#raw"]
        edge = blocks["iii-worker-manager"]
        http = (checkout / "config/iii-http.yaml").read_text()
        self.assertEqual(scalar(raw, "host", 6), "127.0.0.1")
        self.assertEqual(scalar(edge, "host", 6), "0.0.0.0")
        self.assertEqual(scalar(http, "host", 2), "0.0.0.0")
        for oci in ("podman", "docker"):
            args = STACK.launch_arguments(oci, self.home, "image", "fixture-pin")
            publishes = [args[index + 1].split(":") for index, arg in enumerate(args) if arg == "--publish"]
            self.assertTrue(all(host == "127.0.0.1" and host_port == "" for host, host_port, _port in publishes))
            ports = {port for _host, _host_port, port in publishes}
            self.assertEqual(ports, {scalar(edge, "port", 6), scalar(http, "port", 2)})
            self.assertNotIn(scalar(raw, "port", 6), ports)


    def test_private_log_capture_reports_phase_path_and_sanitized_errors(self):
        with patch.object(STACK.os, "open", side_effect=OSError(13, "provider-SECRET")):
            with self.assertRaisesRegex(STACK.StackError, "Private log capture failed at.*last-boot.log.*13") as caught:
                STACK.save_logs("podman", self.home, "a" * 64)
        self.assertNotIn("SECRET", str(caught.exception))
        with patch.object(STACK.subprocess, "run", side_effect=subprocess.TimeoutExpired(["provider-SECRET"], 15)):
            with self.assertRaisesRegex(STACK.StackError, "Log capture timed out after 15s.*private log:") as caught:
                STACK.save_logs("podman", self.home, "a" * 64)
        self.assertNotIn("SECRET", str(caught.exception))
        with patch.object(STACK.subprocess, "run", return_value=subprocess.CompletedProcess([], 125)):
            with self.assertRaisesRegex(STACK.StackError, "Log capture failed.*exit 125.*private log:"):
                STACK.save_logs("podman", self.home, "a" * 64)
        self.assertEqual((self.home / "last-boot.log").stat().st_mode & 0o777, 0o600)

    def test_invalid_creation_receipt_never_triggers_name_or_label_cleanup(self):
        with patch.object(STACK, "inspect", return_value=self.image()), \
                patch.object(STACK, "command", return_value="agentos-name") as command, \
                patch.object(STACK, "stop") as stop, patch.object(STACK, "save_logs") as logs:
            with self.assertRaisesRegex(STACK.StackError, "no valid immutable ID.*no cleanup by name/label"):
                STACK.start("podman", self.home, "0.23.0")
        self.assertEqual(command.call_count, 1)
        stop.assert_not_called()
        logs.assert_not_called()
        self.assertFalse((self.home / "container.json").exists())

    def test_cleanup_error_is_not_reported_as_success_and_record_is_retained(self):
        value = self.value()
        value["State"] = {"Running": False, "ExitCode": 7}
        def command(args, **_kwargs):
            if args[1] == "run":
                return "a" * 64
            raise STACK.CommandError("rm", 125)
        with patch.object(STACK, "inspect", side_effect=self.inspected(value)), \
                patch.object(STACK, "command", side_effect=command), \
                patch.object(STACK, "save_logs", return_value=self.home / "last-boot.log"):
            with self.assertRaisesRegex(STACK.StackError, "exit 7.*Cleanup failed: OCI rm failed.*exit 125") as caught:
                STACK.start("podman", self.home, "0.23.0")
        self.assertNotIn("stopped and removed", str(caught.exception))
        self.assertTrue((self.home / "container.json").exists())

    def test_name_conflict_has_safe_manual_hint_without_discovery(self):
        result = subprocess.CompletedProcess([], 125, "", "name is already in use provider-SECRET")
        with patch.object(STACK.subprocess, "run", return_value=result):
            with self.assertRaisesRegex(STACK.StackError, "name conflict; no adoption/removal by name") as caught:
                STACK.command(["podman", "run", "image"])
        self.assertNotIn("SECRET", str(caught.exception))


if __name__ == "__main__":
    unittest.main()
