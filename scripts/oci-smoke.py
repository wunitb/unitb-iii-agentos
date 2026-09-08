#!/usr/bin/env python3
"""OCI acceptance, with a test-only image layered on the immutable product image.

The fake Anthropic protocol and chat assertions mirror e2e/full-stack.test.ts.
No fixture is added to the production Containerfile, no host fixture port is
published, and no caller-provided provider credential enters the fake lane.
"""
from __future__ import annotations

import argparse
import asyncio
from collections.abc import Iterator
import contextlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import shutil
import shlex
import signal
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import urlsplit
from urllib.request import ProxyHandler, build_opener

ROOT = Path(__file__).resolve().parent.parent
CONTAINER_HOME = Path('/home/agentos/.agentos')
SCRIPT = '/opt/agentos/oci-smoke.py'
UNTRUSTED_DENIED_FUNCTION_IDS = {
    'agentos::bus_auth', 'mcp::list_connections', 'bridge::list', 'configuration::set',
    'agent::chat', 'integration::add', 'integration::remove', 'pulse::register',
    'pulse::invoke', 'pulse::status', 'pulse::toggle', 'swarm::create',
    'swarm::broadcast', 'swarm::dissolve', 'a2a::handle_task', 'compose::status',
}
WORKER_MUTATION_FUNCTION_IDS = {
    'worker::add', 'worker::clear', 'worker::remove', 'worker::start', 'worker::stop', 'worker::update',
}
FAKE_KEY = 'agentos-e2e-fake-anthropic-key'
MODEL = 'claude-haiku-4-5-20251001'
ANSWER = 'deterministic fake-provider answer'
MESSAGE = 'Reply with the deterministic fake-provider answer.'
ENGINE_LABEL = 'io.unitb.agentos.engine'
REQUIRED = {
    'agentos::llm::complete', 'agentos::llm::route', 'agent::chat',
    'memory::recall', 'context::build_prompt', 'cron::create',
    'state::set', 'state::get', 'state::list', 'health::check',
    'engine::queue::enqueue', 'configuration::get',
    'realm::create', 'mission::create', 'security::scan_injection',
    'wasm::execute', 'wasm::list_modules',
}


class SmokeError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SmokeError(message)


def private_write(path: Path, text: str) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, 'w') as stream:
        stream.write(text)


def fixture_diagnostics(home: Path) -> None:
    # Only the hermetic fixture calls this; live-provider logs remain private.
    try:
        secrets = {FAKE_KEY}
        dotenv = home / 'runtime/.env'
        for line in dotenv.read_text().splitlines() if dotenv.exists() else []:
            if not line.strip() or line.lstrip().startswith('#'):
                continue
            key, separator, value = line.partition('=')
            require(bool(separator) and bool(key.strip()), 'invalid fixture dotenv')
            secrets.update(shlex.split(value, comments=True))
        for path in [home / 'last-boot.log', *sorted((home / 'logs').glob('*.log'))]:
            if path.is_file():
                content = path.read_text(errors='replace')
                for secret in sorted(filter(None, secrets), key=len, reverse=True):
                    content = content.replace(secret, '[REDACTED]')
                print(f'OCI fixture diagnostic: {path.name}\n{content[-16000:]}', file=sys.stderr)
    except (OSError, ValueError, SmokeError):
        print('OCI fixture diagnostics unavailable; private logs retained', file=sys.stderr)


def run(args: list[str], env: dict[str, str], *, timeout: int = 600) -> str:
    result = subprocess.run(args, env=env, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, timeout=timeout, check=False)
    if result.returncode:
        # Commands never contain credentials. Logs remain in the scratch home.
        raise SmokeError(f'{Path(args[0]).name} {args[1]} exited {result.returncode}: {result.stderr.strip()}')
    return result.stdout


def fixture_server(home: Path) -> ThreadingHTTPServer:
    lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers.get('content-length', '0'))))
            record = {'method': self.command, 'url': self.path, 'headers': dict(self.headers),
                      'remoteAddress': self.client_address[0], 'body': body}
            with lock:
                with (home / 'provider-requests.jsonl').open('a') as stream:
                    stream.write(json.dumps(record) + '\n')
            response = json.dumps({
                'id': 'msg-agentos-e2e-fake', 'type': 'message', 'role': 'assistant',
                'content': [{'type': 'text', 'text': ANSWER}], 'model': MODEL,
                'stop_reason': 'end_turn', 'stop_sequence': None,
                'usage': {'input_tokens': 7, 'output_tokens': 4},
            }).encode()
            self.send_response(200)
            self.send_header('content-type', 'application/json')
            self.send_header('content-length', str(len(response)))
            self.send_header('connection', 'close')
            self.end_headers()
            self.wfile.write(response)

    return ThreadingHTTPServer(('127.0.0.1', 0), Handler)


def fixture_entrypoint() -> int:
    os.umask(0o077)
    home = Path(os.environ['AGENTOS_HOME'])
    require((home / 'oci-smoke.marker').read_text() == 'fixture-only\n', 'missing scratch fixture marker')
    server = fixture_server(home)
    port = server.server_address[1]
    private_write(home / 'provider.json', json.dumps({'url': f'http://127.0.0.1:{port}', 'port': port}))
    private_write(home / 'provider-requests.jsonl', '')
    runtime = home / 'runtime'
    runtime.mkdir(mode=0o700, exist_ok=True)
    pin = runtime / '.iii-version'
    if not pin.exists():
        require(not any(runtime.iterdir()), 'unversioned fixture runtime must be empty')
        shutil.copyfile('/opt/agentos/runtime/.iii-version', pin)
    dotenv = runtime / '.env'
    lines = dotenv.read_text().splitlines() if dotenv.exists() else []
    # Restart updates only fixture-owned provider settings; product-generated API
    # credentials and persisted data survive. This image refuses non-fixture homes.
    lines = [line for line in lines if not line.startswith(('ANTHROPIC_API_KEY=', 'AGENTOS_ANTHROPIC_BASE_URL='))]
    lines += [f'ANTHROPIC_API_KEY={FAKE_KEY}', f'AGENTOS_ANTHROPIC_BASE_URL=http://127.0.0.1:{port}']
    private_write(dotenv, '\n'.join(lines) + '\n')
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    child = subprocess.Popen(['python3', '/opt/agentos/container-entrypoint.py'])
    previous = {}
    for sig in (signal.SIGTERM, signal.SIGINT):
        previous[sig] = signal.signal(sig, lambda signum, _frame: child.send_signal(signum))
    try:
        return child.wait()
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=45)
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
        for sig, handler in previous.items():
            signal.signal(sig, handler)


def read_api_key(home: Path) -> str:
    values = [line.partition('=')[2].strip().strip('"\'')
              for line in (home / 'runtime/.env').read_text().splitlines()
              if line.startswith('AGENTOS_API_KEY=')]
    require(len(values) == 1 and bool(values[0]), 'product did not generate a unique API credential')
    audit = [line.partition('=')[2].strip().strip("\"'")
             for line in (home / 'runtime/.env').read_text().splitlines()
             if line.startswith('AUDIT_HMAC_KEY=')]
    require(len(audit) == 1 and len(audit[0]) >= 32 and audit[0] != values[0],
            'product did not generate a distinct audit credential')
    require((home / 'runtime/.env').stat().st_mode & 0o077 == 0, 'runtime dotenv is not private')
    return values[0]


def validate_request(requests: list[dict], port: int) -> None:
    require(len(requests) == 1, f'expected exactly one fake provider request, got {len(requests)}')
    request = requests[0]
    headers = {key.lower(): value for key, value in request['headers'].items()}
    require(request['method'] == 'POST' and request['url'] == '/v1/messages', 'wrong provider method/path')
    require(headers.get('host') == f'127.0.0.1:{port}', 'provider Host escaped fixture loopback')
    require(request['remoteAddress'] == '127.0.0.1', 'provider peer is not loopback')
    require(headers.get('x-api-key') == FAKE_KEY, 'provider credential is not the fake key')
    require(headers.get('anthropic-version') == '2023-06-01', 'wrong Anthropic protocol version')
    require(request['body']['model'] == MODEL, 'wrong fake model')
    require(request['body']['messages'][-1] == {'role': 'user', 'content': MESSAGE}, 'wrong provider message/history')


def validate_registry(registry: dict, expected: set[str]) -> None:
    functions = registry.get("functions")
    require(isinstance(functions, list), "registry has no functions array")
    ids = {item.get("function_id") for item in functions}
    require(REQUIRED <= ids, "registry missing required functions: " + ", ".join(sorted(REQUIRED - ids)))
    workers = {item.get("worker_name") for item in functions}
    require(expected <= workers, "registry missing product worker identities: " + ", ".join(sorted(expected - workers)))
    queue = [item for item in functions if item.get("function_id") == "engine::queue::enqueue"]
    require(len(queue) == 1 and queue[0].get("worker_name") == "queue", "queue primitive is not owned by the pinned Compose worker")
    require(not any(item.get("function_id", "").startswith("sandbox::") and
                    str(item.get("worker_name", "")).startswith("agentos-") for item in functions),
            "product worker shadows the builtin sandbox namespace")


def validate_access(authenticated: dict, untrusted: dict) -> None:
    auth_ids = {item.get("function_id") for item in authenticated.get("functions", [])}
    public_ids = {item.get("function_id") for item in untrusted.get("functions", [])}
    require(UNTRUSTED_DENIED_FUNCTION_IDS <= auth_ids, "denial targets must exist in authenticated inventory")
    require("state::get" in public_ids, "untrusted inventory did not provide its allowed view")
    require(not (UNTRUSTED_DENIED_FUNCTION_IDS & public_ids), "sensitive functions leaked into untrusted inventory")
    require(not (WORKER_MUTATION_FUNCTION_IDS & auth_ids), "builtin worker mutation daemons were enabled")

async def inside_acceptance() -> None:
    # Import only in the container: no host SDK or provider dependencies required.
    from iii import InitOptions, register_worker

    home = Path(os.environ['AGENTOS_HOME'])
    require((home / 'oci-smoke.marker').read_text() == 'fixture-only\n', 'acceptance requires fixture home')
    key = read_api_key(home)
    client = register_worker('ws://127.0.0.1:49134', InitOptions(
        worker_name='oci-smoke', headers={'Authorization': f'Bearer {key}'},
        otel={'enabled': False}, enable_metrics_reporting=False))

    async def call(function: str, payload: dict) -> dict:
        # register_worker owns its own loop; schedule through the synchronous bridge.
        return await asyncio.wait_for(asyncio.to_thread(client.trigger, {
            'function_id': function, 'payload': payload, 'timeout_ms': 30000,
        }), timeout=40)

    headers = {'authorization': f'Bearer {key}'}
    try:
        registry = await call('engine::functions::list', {'include_internal': True})
        expected = {'agentos-' + path.name for path in (home / 'runtime/workers').iterdir()
                    if path.is_dir() and (home / 'runtime/target/release' / ('agentos-' + path.name)).exists()}
        require(bool(expected), 'no packaged product worker identities discovered')
        validate_registry(registry, expected)
        untrusted = register_worker("ws://127.0.0.1:49134", InitOptions(
            worker_name="oci-untrusted-fixture", headers={},
            otel={"enabled": False}, enable_metrics_reporting=False))
        try:
            public = await asyncio.wait_for(asyncio.to_thread(untrusted.trigger, {
                "function_id": "engine::functions::list", "payload": {"include_internal": True},
                "timeout_ms": 10000,
            }), timeout=15)
            validate_access(registry, public)
        finally:
            await asyncio.wait_for(asyncio.to_thread(untrusted.shutdown), timeout=15)
        agent_id = 'oci-smoke-fixture-agent'
        created = await call('agent::create', {'headers': headers, 'body': {
            'id': agent_id, 'name': agent_id, 'capabilities': {'functions': []}}})
        require(created.get('agentId') == agent_id, 'agent creation did not return fixture identity')
        try:
            response = await call('agent::chat', {'agentId': agent_id, 'headers': headers,
                'sessionId': 'oci-smoke-fixture-session', 'provider': 'anthropic',
                'model': MODEL, 'message': MESSAGE})
            require(response.get('content') == ANSWER, 'agent chat did not use deterministic provider')
            requests = [json.loads(line) for line in (home / 'provider-requests.jsonl').read_text().splitlines()]
            validate_request(requests, json.loads((home / 'provider.json').read_text())['port'])
        finally:
            deleted = await call('agent::delete', {'headers': headers, 'agentId': agent_id})
            require(deleted.get('deleted') is True, 'fixture agent cleanup failed')
        realm = await call('realm::create', {'name': 'oci-smoke-realm', 'owner': 'oci-smoke'})
        require(bool(realm.get('id') or realm.get('realmId')), 'realm creation failed')
        realm_id = realm.get('id') or realm.get('realmId')
        mission = await call('mission::create', {'realmId': realm_id, 'title': 'OCI fixture mission',
                                                'description': 'credential-free acceptance', 'createdBy': 'oci-smoke'})
        require(bool(mission.get('id') or mission.get('missionId')), 'mission creation failed')
        scan = await call('security::scan_injection', {'text': 'A harmless fixture greeting.'})
        require(isinstance(scan, dict) and bool(scan), 'security scan returned no result')
        modules = await call('wasm::list_modules', {})
        require(isinstance(modules.get('modules'), list) and isinstance(modules.get('count'), int), 'wasm inventory failed')
        await call('state::set', {'scope': 'oci-smoke', 'key': 'persistence', 'value': {'sentinel': 'preserve-me'}})
        private_write(home / 'acceptance.json', json.dumps({'checks': [
            'registry', 'worker-identities', 'access-control', 'fake-chat', 'fake-protocol', 'realm', 'mission', 'security', 'wasm'],
            'passed': True, 'provider_requests': len(requests)}))
    finally:
        await asyncio.wait_for(asyncio.to_thread(client.shutdown), timeout=15)


def validate_status(value: dict, engine: str) -> str:
    require(value.get('running') is True, 'owned OCI container is not running')
    require(value.get('ready') is True, 'owned OCI container is not ready')
    require(value.get('engine') == engine, 'wrong OCI engine pin')
    endpoints = value.get('endpoints', {})
    require(set(endpoints) == {'api', 'bus'}, 'missing or unexpected published endpoints')
    for name, scheme in (('api', 'http'), ('bus', 'ws')):
        url = urlsplit(endpoints[name])
        require(url.scheme == scheme and url.hostname == '127.0.0.1' and url.port is not None
                and not url.username and not url.password, 'endpoint is not runtime-assigned loopback')
    return endpoints['api']


@contextlib.contextmanager
def fixture_base_reference(oci: str, identity: str, reference: str,
                           env: dict[str, str]) -> Iterator[str]:
    # BuildKit treats a bare image ID in FROM as a registry repository name.
    # Give the inspected image a private, temporary name without replacing its tags.
    existing = run([oci, 'image', 'ls', '--quiet', '--no-trunc',
                    '--filter', f'reference={reference}'], env, timeout=30)
    require(not existing.strip(), 'fixture base reference already exists')
    source = json.loads(run([oci, 'image', 'inspect', identity], env, timeout=30))[0]
    require(any(tag and tag != '<none>:<none>' for tag in source.get('RepoTags') or []),
            'fixture requires a retained production image tag')
    run([oci, 'image', 'tag', identity, reference], env, timeout=30)
    try:
        pinned = json.loads(run([oci, 'image', 'inspect', reference], env, timeout=30))[0]
        require(pinned['Id'] == identity, 'fixture base identity changed')
        yield reference
    finally:
        pinned = json.loads(run([oci, 'image', 'inspect', reference], env, timeout=30))[0]
        require(pinned['Id'] == identity, 'fixture base identity changed; reference retained')
        require(any(tag and tag not in (reference, '<none>:<none>')
                    for tag in pinned.get('RepoTags') or []),
                'refusing to remove the last production image tag; reference retained')
        run([oci, 'image', 'rm', reference], env, timeout=30)


def host_acceptance(*, build: bool = True, live: bool = False, report: Path | None = None) -> None:
    launcher = ROOT / 'scripts/oci-stack.sh'
    require(launcher.is_file(), 'complete OCI checkout required (scripts/oci-stack.sh missing)')
    engine = (ROOT / '.iii-version').read_text().strip()
    require(engine == '0.23.0', 'OCI acceptance requires the checked-out stable iii 0.23.0 pin')
    oci = os.environ.get('AGENTOS_OCI_RUNTIME')
    if not oci:
        oci = next((path for name in ('podman', 'docker') if (path := shutil.which(name))), None)
    require(bool(oci), 'running Podman or Docker required')
    scratch = Path(tempfile.mkdtemp(prefix='agentos-oci-smoke-'))
    home = scratch / 'home'
    home.mkdir(mode=0o700)
    os.umask(0o077)
    # Keep only runtime/tool transport, never ambient dotenv/provider/proxy vars.
    env = {key: os.environ[key] for key in ('PATH', 'XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS',
           'DOCKER_HOST', 'CONTAINER_HOST', 'SSL_CERT_FILE', 'SSL_CERT_DIR') if key in os.environ}
    # OCI clients locate their existing image store under the operator home.
    # Only the container's state/credentials belong in the isolated test home.
    env.update(HOME=str(Path.home()), AGENTOS_OCI_HOME=str(home), AGENTOS_OCI_RUNTIME=str(oci))
    base = os.environ.get('AGENTOS_OCI_IMAGE', 'localhost/unitb-agentos:local')
    env['AGENTOS_OCI_IMAGE'] = base
    image = f'localhost/agentos-oci-smoke:{scratch.name.removeprefix("agentos-oci-smoke-")}'
    image_created = False
    start_attempted = False
    success = False
    receipt = {'schema': 'agentos-oci-acceptance/v1', 'mode': 'live' if live else 'fixture', 'engine': engine}

    def stack(*args: str, timeout: int = 600) -> str:
        return run(['bash', str(launcher), *args], env, timeout=timeout)

    def health() -> dict:
        value = json.loads(stack('status'))
        api = validate_status(value, engine)
        with build_opener(ProxyHandler({})).open(api + '/api/health', timeout=15) as response:
            require(response.status == 200, 'HTTP health failed')
            require(isinstance(json.load(response), dict), 'HTTP health is not JSON')
        return value

    try:
        run([str(oci), 'info'], env, timeout=30)
        if build:
            stack('build', timeout=3600)
        inspected = json.loads(run([str(oci), 'image', 'inspect', base], env))[0]
        require(inspected['Config']['Labels'][ENGINE_LABEL] == engine, 'base image engine pin mismatch')
        identity = inspected['Id']
        require(bool(re.fullmatch(r'(?:sha256:)?[0-9a-f]{64}', identity)), 'base image has no immutable identity')
        if live:
            runtime = home / 'runtime'
            runtime.mkdir(mode=0o700)
            private_write(runtime / '.iii-version', engine + '\n')
            credentials = {}
            for key in ('AGENTOS_API_KEY', 'ANTHROPIC_API_KEY'):
                value = os.environ.get(key, '')
                require(bool(value) and '\n' not in value and '\r' not in value, f'{key} is required for live E2E')
                credentials[key] = value
            private_write(runtime / '.env', ''.join(f'{key}={json.dumps(value)}\n' for key, value in credentials.items()))
        else:
            context = scratch / 'image'
            context.mkdir(mode=0o700)
            shutil.copyfile(Path(__file__), context / 'oci-smoke.py')
            with fixture_base_reference(str(oci), identity, image + '-base', env) as base_reference:
                private_write(context / 'Containerfile', f'FROM {base_reference}\nCOPY --chmod=0644 oci-smoke.py {SCRIPT}\n'
                    f'ENTRYPOINT ["/usr/bin/tini", "--", "python3", "{SCRIPT}", "--fixture-entrypoint"]\n')
                run([str(oci), 'build', '--file', str(context / 'Containerfile'), '--tag', image,
                     '--label', f'{ENGINE_LABEL}={engine}', str(context)], env, timeout=300)
                image_created = True
            env['AGENTOS_OCI_IMAGE'] = image
            private_write(home / 'oci-smoke.marker', 'fixture-only\n')
        start_attempted = True
        stack('up')
        status = health()
        if live:
            test_env = dict(env, **credentials, AGENTOS_E2E='1',
                AGENTOS_BASE_URL=status['endpoints']['api'], III_URL=status['endpoints']['bus'],
                AGENTOS_E2E_MODEL='claude-sonnet-4-20250514')
            run(['bun', 'run', '--cwd', str(ROOT), 'test:e2e'], test_env, timeout=900)
        else:
            stack('exec', 'python3', SCRIPT, '--inside', timeout=180)
            evidence = json.loads((home / 'acceptance.json').read_text())
            require(evidence.get('passed') is True and len(evidence.get('checks', [])) == 9, 'missing acceptance evidence')
            sentinel = home / 'runtime/data/oci-smoke-sentinel'
            sentinel.parent.mkdir(parents=True, exist_ok=True)
            private_write(sentinel, 'preserve-me\n')
            key = read_api_key(home)
            stack('stop')
            require(json.loads(stack('status')).get('running') is False, 'container still running after stop')
            stack('up')
            health()
            require(sentinel.read_text() == 'preserve-me\n', 'restart overwrote runtime data')
            require(read_api_key(home) == key, 'restart overwrote generated API credential')
        receipt.update(image=identity, success=True)
        if not live:
            receipt.update(checks=evidence['checks'], provider_requests=evidence['provider_requests'],
                           restart=True, data_preserved=True, key_preserved=True)
        success = True
    finally:
        try:
            if start_attempted:
                stack('stop')
                require(json.loads(stack('status')).get('running') is False, 'owned teardown failed')
            if image_created:
                run([str(oci), 'image', 'rm', image], env)
        except Exception:
            print(f'OCI smoke: cleanup failed; private recovery evidence retained: {scratch}', file=sys.stderr)
            raise
        if success:
            receipt['teardown'] = True
            if report is not None:
                report.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                private_write(report, json.dumps(receipt, indent=2) + '\n')
            shutil.rmtree(scratch)
        else:
            if not live:
                fixture_diagnostics(home)
            print(f'OCI smoke: failed; private scratch evidence retained: {scratch}', file=sys.stderr)
    print(json.dumps(receipt))
    print('OCI smoke: real OCI readiness, acceptance and owned teardown passed')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--no-build', action='store_true', help='use an already-built, engine-pin-checked production image')
    parser.add_argument('--report', type=Path, help='write credential-free acceptance JSON only after verified teardown')
    parser.add_argument('--live-e2e', action='store_true', help='explicit opt-in: require live credentials and run full E2E')
    parser.add_argument('--fixture-entrypoint', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--inside', action='store_true', help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.fixture_entrypoint:
        sys.exit(fixture_entrypoint())
    if args.inside:
        asyncio.run(inside_acceptance())
    else:
        host_acceptance(build=not args.no_build, live=args.live_e2e, report=args.report)


if __name__ == '__main__':
    try:
        main()
    except (SmokeError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        print(f'OCI smoke: {error}', file=sys.stderr)
        sys.exit(1)
