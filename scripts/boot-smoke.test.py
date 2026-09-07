#!/usr/bin/env python3
import importlib.util
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


if __name__ == "__main__":
    result = unittest.main(exit=False).result
    if not result.wasSuccessful() or result.testsRun == 0:
        raise SystemExit(1)
    print("OCI smoke unit checks passed")
