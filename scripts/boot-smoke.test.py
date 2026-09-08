#!/usr/bin/env python3
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

SPEC = importlib.util.spec_from_file_location("oci_smoke", Path(__file__).with_name("oci-smoke.py"))
SMOKE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(SMOKE)


class OciSmokeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.home = Path(temporary.name)

    def request(self):
        return {"method": "POST", "url": "/v1/messages", "remoteAddress": "127.0.0.1",
                "headers": {"host": "127.0.0.1:12001", "x-api-key": SMOKE.FAKE_KEY,
                            "anthropic-version": "2023-06-01"},
                "body": {"model": SMOKE.MODEL, "messages": [{"role": "user", "content": SMOKE.MESSAGE}]}}

    def status(self):
        return {"running": True, "ready": True, "engine": "0.23.0",
                "endpoints": {"api": "http://127.0.0.1:12001", "bus": "ws://127.0.0.1:12002"}}

    def test_fake_protocol_checks_actual_request_not_just_chat_text(self):
        SMOKE.validate_request([self.request()], 12001)
        for field, value in (("url", "/wrong"), ("remoteAddress", "192.0.2.1")):
            request = self.request()
            request[field] = value
            with self.assertRaises(SMOKE.SmokeError):
                SMOKE.validate_request([request], 12001)
        request = self.request()
        request["headers"]["x-api-key"] = "not-the-fixture"
        with self.assertRaises(SMOKE.SmokeError):
            SMOKE.validate_request([request], 12001)

    def test_request_cardinality_cannot_hide_duplicate_provider_calls(self):
        for requests in ([], [self.request(), self.request()]):
            with self.assertRaises(SMOKE.SmokeError):
                SMOKE.validate_request(requests, 12001)

    def test_registry_requires_every_function_and_product_identity(self):
        functions = [{"function_id": name, "worker_name": "queue" if name == "engine::queue::enqueue" else "agentos-fixture"} for name in SMOKE.REQUIRED]
        SMOKE.validate_registry({"functions": functions}, {"agentos-fixture"})
        with self.assertRaises(SMOKE.SmokeError):
            SMOKE.validate_registry({"functions": functions[1:]}, {"agentos-fixture"})
        with self.assertRaises(SMOKE.SmokeError):
            SMOKE.validate_registry({"functions": functions}, {"agentos-missing"})

    def test_status_needs_readiness_and_loopback_only_endpoints(self):
        self.assertEqual(SMOKE.validate_status(self.status(), "0.23.0"), "http://127.0.0.1:12001")
        status = self.status()
        status["ready"] = False
        with self.assertRaises(SMOKE.SmokeError):
            SMOKE.validate_status(status, "0.23.0")
        status = self.status()
        status["endpoints"]["api"] = "http://0.0.0.0:12001"
        with self.assertRaises(SMOKE.SmokeError):
            SMOKE.validate_status(status, "0.23.0")

    def test_command_failures_propagate(self):
        with self.assertRaisesRegex(SMOKE.SmokeError, "exited 7"):
            SMOKE.run(["python3", "-c", "raise SystemExit(7)"], {"PATH": os.environ["PATH"]}, timeout=5)

    def test_access_views_require_real_targets_and_hide_denied_functions(self):
        authenticated = {"functions": [{"function_id": name} for name in SMOKE.UNTRUSTED_DENIED_FUNCTION_IDS]}
        public = {"functions": [{"function_id": "state::get"}]}
        SMOKE.validate_access(authenticated, public)
        with self.assertRaises(SMOKE.SmokeError):
            SMOKE.validate_access({"functions": []}, public)
        public["functions"].append({"function_id": "compose::status"})
        with self.assertRaises(SMOKE.SmokeError):
            SMOKE.validate_access(authenticated, public)

    def test_fixture_seeds_only_its_empty_runtime_with_image_pin(self):
        (self.home / "oci-smoke.marker").write_text("fixture-only\n")
        server = Mock(server_address=("127.0.0.1", 12001))
        child = Mock()
        child.wait.return_value = 0
        child.poll.return_value = 0
        def copy_pin(source, destination):
            self.assertEqual(source, "/opt/agentos/runtime/.iii-version")
            Path(destination).write_text("0.23.0\n")
        with patch.dict(os.environ, {"AGENTOS_HOME": str(self.home)}), \
             patch.object(SMOKE, "fixture_server", return_value=server), \
             patch.object(SMOKE.shutil, "copyfile", side_effect=copy_pin), \
             patch.object(SMOKE.subprocess, "Popen", return_value=child):
            self.assertEqual(SMOKE.fixture_entrypoint(), 0)
        self.assertEqual((self.home / "runtime/.iii-version").read_text(), "0.23.0\n")
        self.assertIn(SMOKE.FAKE_KEY, (self.home / "runtime/.env").read_text())
        server.shutdown.assert_called_once()
        server.server_close.assert_called_once()


class FixtureBaseTests(unittest.TestCase):
    def setUp(self):
        self.identity = "sha256:" + "a" * 64
        self.production = "localhost/unitb-agentos:local"
        self.reference = "localhost/agentos-oci-smoke:unit-base"

    def image(self, tags, identity=None):
        return json.dumps([{"Id": identity or self.identity, "RepoTags": tags,
                            "Config": {"Labels": {SMOKE.ENGINE_LABEL: "0.23.0"}}}])

    def replies(self):
        pinned = self.image([self.production, self.reference])
        return ["", self.image([self.production]), "", pinned, pinned, ""]

    def reference_context(self):
        return SMOKE.fixture_base_reference("docker", self.identity, self.reference, {})

    def test_named_reference_is_bound_to_docker_and_podman_image_ids(self):
        for identity in ("sha256:" + "a" * 64, "a" * 64):
            with self.subTest(identity=identity):
                self.identity = identity
                with patch.object(SMOKE, "run", side_effect=self.replies()) as run:
                    with self.reference_context() as reference:
                        self.assertEqual(reference, self.reference)
                    self.assertEqual(run.call_args_list[2].args[0],
                                     ["docker", "image", "tag", identity, self.reference])
                    self.assertEqual(run.call_args_list[-1].args[0],
                                     ["docker", "image", "rm", self.reference])
                    self.assertEqual(run.call_count, 6)

    def test_existing_reference_is_never_overwritten_or_removed(self):
        with patch.object(SMOKE, "run", return_value=self.identity) as run:
            with self.assertRaisesRegex(SMOKE.SmokeError, "already exists"):
                with self.reference_context():
                    self.fail("must refuse the collision before tagging")
            self.assertEqual(run.call_count, 1)

    def test_untagged_production_image_is_not_adopted(self):
        for tags in (None, [], ["<none>:<none>"]):
            with self.subTest(tags=tags), patch.object(SMOKE, "run", side_effect=["", self.image(tags)]) as run:
                with self.assertRaisesRegex(SMOKE.SmokeError, "retained production image tag"):
                    with self.reference_context():
                        self.fail("must preserve an untagged production image")
                self.assertEqual(run.call_count, 2)

    def test_build_failure_cleans_only_the_owned_reference(self):
        with patch.object(SMOKE, "run", side_effect=self.replies()) as run:
            with self.assertRaisesRegex(SMOKE.SmokeError, "fixture build failed"):
                with self.reference_context():
                    raise SMOKE.SmokeError("fixture build failed")
            self.assertEqual(run.call_args_list[-1].args[0],
                             ["docker", "image", "rm", self.reference])

    def test_reference_drift_is_not_removed(self):
        replies = self.replies()
        replies[4] = self.image([self.production, self.reference], "sha256:" + "b" * 64)
        with patch.object(SMOKE, "run", side_effect=replies) as run:
            with self.assertRaisesRegex(SMOKE.SmokeError, "identity changed; reference retained"):
                with self.reference_context():
                    pass
            self.assertEqual(run.call_count, 5)

    def test_loss_of_production_tag_retains_the_image(self):
        replies = self.replies()
        replies[4] = self.image([self.reference])
        with patch.object(SMOKE, "run", side_effect=replies) as run:
            with self.assertRaisesRegex(SMOKE.SmokeError, "last production image tag"):
                with self.reference_context():
                    pass
            self.assertEqual(run.call_count, 5)

    def test_tag_identity_is_checked_before_fixture_build(self):
        replies = self.replies()
        changed = self.image([self.production, self.reference], "sha256:" + "b" * 64)
        replies[3:5] = [changed, changed]
        with patch.object(SMOKE, "run", side_effect=replies) as run:
            with self.assertRaisesRegex(SMOKE.SmokeError, "identity changed"):
                with self.reference_context():
                    self.fail("must not build from a changed reference")
            self.assertEqual(run.call_count, 5)

    def test_cleanup_failure_does_not_report_success(self):
        replies = self.replies()
        replies[-1] = SMOKE.SmokeError("cleanup failed")
        with patch.object(SMOKE, "run", side_effect=replies):
            with self.assertRaisesRegex(SMOKE.SmokeError, "cleanup failed"):
                with self.reference_context():
                    pass

    def test_host_build_uses_a_named_local_base_not_a_bare_image_id(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "scripts").mkdir()
            (root / "scripts/oci-stack.sh").write_text("fixture-launcher")
            (root / ".iii-version").write_text("0.23.0\n")
            scratch = root / "agentos-oci-smoke-unit"
            scratch.mkdir()
            references = []
            removed = []

            def invoke(args, _env, **_kwargs):
                if args[1] == "info":
                    return "{}"
                if args[1:3] == ["image", "ls"]:
                    return ""
                if args[1:3] == ["image", "inspect"]:
                    return self.image([self.production] + references)
                if args[1:3] == ["image", "tag"]:
                    self.assertEqual(args[3], self.identity)
                    references.append(args[4])
                    return ""
                if args[1:3] == ["image", "rm"]:
                    removed.append(args[3])
                    return ""
                if args[1] == "build":
                    containerfile = Path(args[args.index("--file") + 1]).read_text()
                    self.assertTrue(containerfile.startswith(f"FROM {references[0]}\n"))
                    self.assertNotIn(f"FROM {self.identity}", containerfile)
                    raise SMOKE.SmokeError("checked BuildKit input")
                self.fail(f"unexpected fixture command: {args}")

            with patch.dict(os.environ, {"AGENTOS_OCI_RUNTIME": "docker",
                                         "AGENTOS_OCI_IMAGE": self.production, "HOME": str(root)}, clear=True), \
                 patch.object(SMOKE, "ROOT", root), \
                 patch.object(SMOKE.tempfile, "mkdtemp", return_value=str(scratch)), \
                 patch.object(SMOKE, "run", side_effect=invoke), \
                 patch.object(SMOKE.sys, "stderr", io.StringIO()):
                with self.assertRaisesRegex(SMOKE.SmokeError, "checked BuildKit input"):
                    SMOKE.host_acceptance(build=False)
            self.assertEqual(len(references), 1)
            self.assertEqual(removed, references)


if __name__ == "__main__":
    result = unittest.main(exit=False).result
    if not result.wasSuccessful() or result.testsRun == 0:
        raise SystemExit(1)
    print("OCI smoke unit checks passed")
