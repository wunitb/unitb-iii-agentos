"""Check the released SDK contract without connecting a worker or loading a model."""

import os
from pathlib import Path
import subprocess
import sys

import pytest


@pytest.mark.parametrize("key", [None, "", "fixture-bus-key"])
def test_released_sdk_registration_contract(key):
    # test_main.py intentionally mocks `iii` at import time. A clean subprocess
    # proves these options against the real installed SDK, not that mock.
    env = {"PATH": os.environ["PATH"]}
    if key is not None:
        env["AGENTOS_API_KEY"] = key
    source = """
import importlib.metadata
import os
import sys
from unittest.mock import create_autospec, patch
from iii import InitOptions
from iii.iii import III

assert importlib.metadata.version("iii-sdk") == "0.23.0"
client = create_autospec(III, instance=True)
with patch("iii.register_worker", return_value=client) as register:
    import main
register.assert_called_once()
address, options = register.call_args.args
assert address == "ws://localhost:49134"
assert isinstance(options, InitOptions)
assert options.worker_name == "embedding"
assert options.namespace is None  # inherit SDK/engine namespace, never force a new one
key = os.getenv("AGENTOS_API_KEY")
assert options.headers == ({"Authorization": f"Bearer {key}"} if key else None)
assert options.otel is None  # keep the SDK's telemetry defaults
assert [call.args[0] for call in client.register_function.call_args_list] == [
    "embedding::generate", "embedding::similarity",
]
assert client.register_function.call_args_list[0].args[1] is main.generate_embedding
assert client.register_function.call_args_list[1].args[1] is main.compute_similarity
with patch.dict(sys.modules, {"sentence_transformers": None}):
    assert main.get_model() == "fallback"
print("AGENTOS_PYTHON_SDK_CONTRACT_OK")
"""
    result = subprocess.run(
        [sys.executable, "-c", source],
        cwd=Path(__file__).parent,
        env=env,
        capture_output=True,
        text=True,
        timeout=15,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert result.stderr == ""
    assert "AGENTOS_PYTHON_SDK_CONTRACT_OK" in result.stdout
