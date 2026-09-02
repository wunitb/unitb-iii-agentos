//! End-to-end behaviour of `agentos up` against fake engine, worker, and TUI
//! binaries. The engine port is shared machine state, so every test that
//! depends on it takes `ENGINE_PORT_LOCK`.

#![cfg(unix)]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const ENGINE_PORT: u16 = 49134;

static ENGINE_PORT_LOCK: Mutex<()> = Mutex::new(());

fn engine_port_lock() -> MutexGuard<'static, ()> {
    ENGINE_PORT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn temporary_directory(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("agentos-up-{label}-{}-{stamp}", std::process::id()));
    fs::create_dir_all(&path).expect("create temporary directory");
    path
}

fn write_executable(path: &Path, body: &str) {
    fs::write(path, body).expect("write executable");
    let mut permissions = fs::metadata(path)
        .expect("read executable metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("make executable");
}

fn wait_for_file(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.is_file() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

/// A fresh-clone-equivalent layout: runtime config, one worker manifest with a
/// release binary, a fake engine, and a fake TUI.
struct Fixture {
    root: PathBuf,
    home: PathBuf,
    runtime: PathBuf,
    bin: PathBuf,
    engine_marker: PathBuf,
    worker_pid: PathBuf,
    worker_env: PathBuf,
    release: PathBuf,
    tui_marker: PathBuf,
    tui_env: PathBuf,
    bus_auth_env: PathBuf,
    bus_auth_pid: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = temporary_directory(label);
        let home = root.join("home");
        let runtime = root.join("runtime");
        let bin = root.join("bin");
        let release = runtime.join("target/release");
        fs::create_dir_all(runtime.join("workers/echo")).expect("create worker directory");
        fs::create_dir_all(&release).expect("create release directory");
        fs::create_dir_all(&bin).expect("create bin directory");
        fs::create_dir_all(&home).expect("create home directory");
        fs::write(runtime.join("config.yaml"), "workers: []\n").expect("write runtime config");
        fs::write(
            runtime.join(".env"),
            "DOTENV_ONLY=from-dotenv\nEXPLICIT_WINS=from-dotenv\nAGENTOS_API_KEY=fresh-clone-key\n",
        )
        .expect("write dotenv file");
        fs::write(
            runtime.join("workers/echo/iii.worker.yaml"),
            "iii: v1\nname: echo\nruntime: rust\nscripts:\n  start: echo\n",
        )
        .expect("write worker manifest");

        let engine_marker = root.join("engine.started");
        let worker_pid = root.join("worker.pid");
        let worker_env = root.join("worker.env");
        let tui_marker = root.join("tui.started");
        let tui_env = root.join("tui.env");
        let bus_auth_env = root.join("bus-auth.env");
        let bus_auth_pid = root.join("bus-auth.pid");
        write_executable(
            &bin.join("iii"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'iii 0.22.1'; exit 0; fi\nif [ \"$1\" = \"trigger\" ]; then if [ -f '{}' ]; then printf '%s\\n' '{{\"workers\":[{{\"name\":\"echo\",\"runtime\":\"rust\",\"status\":\"connected\"}}]}}'; else printf '%s\\n' '{{\"workers\":[]}}'; fi; exit 0; fi\necho started > '{}'\nexec sleep 60\n",
                worker_pid.display(),
                engine_marker.display()
            ),
        );
        write_executable(
            &release.join("agentos-echo"),
            &format!(
                "#!/bin/sh\necho $$ > '{}'\nprintf '%s|%s|%s\\n' \"$DOTENV_ONLY\" \"$EXPLICIT_WINS\" \"$III_WORKER_NAME\" > '{}'\nexec sleep 60\n",
                worker_pid.display(),
                worker_env.display()
            ),
        );

        Self {
            root,
            home,
            runtime,
            bin,
            engine_marker,
            worker_pid,
            worker_env,
            release,
            tui_marker,
            tui_env,
            bus_auth_env,
            bus_auth_pid,
        }
    }

    /// A copy of the CLI inside the fixture, so `current_exe().parent()` is the
    /// fixture's own bin directory. `up` resolves `agentos-tui` and
    /// `agentos-bus-authd` next to the CLI first — product behaviour — and
    /// running the workspace copy would otherwise pick up the real binaries
    /// `cargo test --workspace` builds in `target/debug`.
    fn cli(&self) -> PathBuf {
        let cli = self.bin.join("agentos");
        fs::copy(env!("CARGO_BIN_EXE_agentos"), &cli).expect("copy agentos binary");
        let mut permissions = fs::metadata(&cli).expect("read cli metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&cli, permissions).expect("make cli executable");
        cli
    }

    /// Installs a fake TUI beside a copy of the CLI, mirroring an installed
    /// release layout where `up` finds the TUI next to itself.
    fn with_tui(&self, exit_code: i32) -> PathBuf {
        let cli = self.cli();
        write_executable(
            &self.bin.join("agentos-tui"),
            &format!(
                "#!/bin/sh\necho started > '{}'\nprintf '%s|%s|%s\\n' \"$DOTENV_ONLY\" \"$EXPLICIT_WINS\" \"$AGENTOS_API_KEY\" > '{}'\nexit {exit_code}\n",
                self.tui_marker.display(),
                self.tui_env.display()
            ),
        );
        cli
    }

    /// Replaces the worker with one that dies straight after being spawned.
    fn with_failing_worker(&self) {
        write_executable(&self.release.join("agentos-echo"), "#!/bin/sh\nexit 3\n");
    }

    /// Arms bus RBAC in the runtime config and installs a fake
    /// `agentos-bus-authd` that records its argv and environment, then holds
    /// the port the way the real daemon does.
    fn with_bus_auth(&self, addr: &str) {
        fs::write(
            self.runtime.join("config.yaml"),
            format!(
                "workers:\n  - name: iii-worker-manager\n    config:\n      rbac:\n        auth_function_id: agentos::bus_auth\n  - name: iii-bridge\n    config:\n      url: ws://{addr}\n"
            ),
        )
        .expect("write armed config");
        let port = addr.rsplit(':').next().unwrap_or("0").to_string();
        write_executable(
            &self.release.join("agentos-bus-authd"),
            &format!(
                "#!/bin/sh\nprintf '%s|%s\\n' \"$1\" \"$AGENTOS_API_KEY\" > '{}'\necho $$ > '{}'\nexec python3 -c \"import socket,time\ns=socket.socket()\ns.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)\ns.bind(('127.0.0.1',{port}))\ns.listen()\ntime.sleep(60)\"\n",
                self.bus_auth_env.display(),
                self.bus_auth_pid.display()
            ),
        );
    }

    /// A clean machine: the dotenv ships `AGENTOS_API_KEY` empty, exactly as
    /// `.env.example` does, so nothing can boot until a key exists.
    fn with_empty_api_key(&self) {
        fs::write(
            self.runtime.join(".env"),
            "DOTENV_ONLY=from-dotenv\nEXPLICIT_WINS=from-dotenv\nAGENTOS_API_KEY=\n",
        )
        .expect("write dotenv without a key");
    }

    fn dotenv(&self) -> String {
        fs::read_to_string(self.runtime.join(".env")).expect("read dotenv")
    }

    /// The value assigned to `name` in the fixture dotenv, if any.
    fn dotenv_value(&self, name: &str) -> Option<String> {
        self.dotenv().lines().find_map(|line| {
            line.split_once('=')
                .filter(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    fn dotenv_mode(&self) -> u32 {
        fs::metadata(self.runtime.join(".env"))
            .expect("stat dotenv")
            .permissions()
            .mode()
            & 0o777
    }

    /// `agentos start` runs until interrupted, so the caller gets the child and
    /// stops it once the launched worker has recorded its environment.
    fn start(&self, program: &Path, path: &str) -> std::process::Child {
        Command::new(program)
            .arg("start")
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("AGENTOS_HOME", &self.home)
            .env("AGENTOS_CONFIG", self.runtime.join("config.yaml"))
            .env("EXPLICIT_WINS", "from-shell")
            .env_remove("AGENTOS_API_KEY")
            .current_dir(&self.root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("run agentos start")
    }

    fn up(&self, program: &Path, arguments: &[&str], path: &str) -> Output {
        Command::new(program)
            .arg("up")
            .args(arguments)
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("AGENTOS_HOME", &self.home)
            .env("AGENTOS_CONFIG", self.runtime.join("config.yaml"))
            .env("EXPLICIT_WINS", "from-shell")
            // An inherited key would win over the file and hide whether `up`
            // wrote one, so the fixture always starts without it.
            .env_remove("AGENTOS_API_KEY")
            .current_dir(&self.root)
            .output()
            .expect("run agentos up")
    }

    fn path_with_engine(&self) -> String {
        format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    fn stop_worker(&self) {
        if let Ok(pid) = fs::read_to_string(&self.worker_pid)
            && let Ok(pid) = pid.trim().parse::<i32>()
        {
            terminate(pid);
        }
        // The fake engine API reads this file: removing it reports the worker
        // as gone from the bus.
        let _ = fs::remove_file(&self.worker_pid);
    }

    /// Every process a fixture can leave behind. `up` detaches what it starts,
    /// so a daemon that survives a failed assertion holds its port and breaks
    /// every later run on the machine.
    fn stop_processes(&self) {
        self.stop_worker();
        if let Ok(pid) = fs::read_to_string(&self.bus_auth_pid)
            && let Ok(pid) = pid.trim().parse::<i32>()
        {
            terminate(pid);
        }
    }

    fn cleanup(self) {
        self.stop_processes();
        fs::remove_dir_all(&self.root).expect("remove temporary directory");
    }
}

impl Drop for Fixture {
    /// Runs on the panic path too, which `cleanup()` cannot: a failed
    /// assertion must not leak a detached daemon.
    fn drop(&mut self) {
        self.stop_processes();
    }
}

/// A loopback port nothing is listening on right now. Fixed ports are shared
/// machine state: two runs, or a leaked process from an earlier one, make the
/// daemon assertions test the wrong thing.
fn free_loopback_addr() -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind an ephemeral port");
    let addr = listener.local_addr().expect("read ephemeral port");
    drop(listener);
    addr.to_string()
}

/// `kill -TERM` without pulling in a dependency for one call.
fn terminate(pid: i32) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status();
}

#[test]
fn up_reuses_a_healthy_engine_and_starts_workers_without_the_tui() {
    let _guard = engine_port_lock();
    let fixture = Fixture::new("reuse-engine");
    let Ok(engine) = TcpListener::bind(("127.0.0.1", ENGINE_PORT)) else {
        eprintln!("skipped: port {ENGINE_PORT} is already in use by another engine");
        fixture.cleanup();
        return;
    };

    let cli = PathBuf::from(env!("CARGO_BIN_EXE_agentos"));
    let output = fixture.up(&cli, &["--no-tui"], &fixture.path_with_engine());
    assert!(
        output.status.success(),
        "up failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("already healthy on 127.0.0.1:49134"),
        "{stdout}"
    );
    assert!(stdout.contains("1 started"), "{stdout}");
    assert!(stdout.contains("1 connected"), "{stdout}");
    assert!(wait_for_file(&fixture.worker_pid), "worker never started");
    assert_eq!(
        fs::read_to_string(&fixture.worker_env).expect("read worker environment"),
        "from-dotenv|from-shell|echo\n"
    );
    let first_pid = fs::read_to_string(&fixture.worker_pid).expect("read worker pid");
    assert!(
        !fixture.engine_marker.exists(),
        "up spawned a duplicate engine"
    );

    // A second invocation with the worker already on the bus must start nothing.
    let output = fixture.up(&cli, &["--no-tui"], &fixture.path_with_engine());
    assert!(
        output.status.success(),
        "second up failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("already connected; not starting duplicates"),
        "{stdout}"
    );
    assert_eq!(
        fs::read_to_string(&fixture.worker_pid).expect("read worker pid"),
        first_pid,
        "up started a duplicate worker"
    );
    assert!(
        !fixture.engine_marker.exists(),
        "second up spawned a duplicate engine"
    );

    // Once the worker is gone, the same command restarts it.
    fixture.stop_worker();
    let output = fixture.up(&cli, &["--no-tui"], &fixture.path_with_engine());
    assert!(output.status.success());
    assert!(wait_for_file(&fixture.worker_pid), "worker never restarted");
    assert!(
        !fixture.engine_marker.exists(),
        "third up spawned a duplicate engine"
    );

    drop(engine);
    fixture.cleanup();
}

#[test]
fn up_runs_the_tui_in_the_foreground_and_propagates_its_exit_code() {
    let _guard = engine_port_lock();
    let fixture = Fixture::new("foreground-tui");
    let Ok(engine) = TcpListener::bind(("127.0.0.1", ENGINE_PORT)) else {
        eprintln!("skipped: port {ENGINE_PORT} is already in use by another engine");
        fixture.cleanup();
        return;
    };

    let cli = fixture.with_tui(7);
    let output = fixture.up(&cli, &[], &fixture.path_with_engine());
    assert_eq!(output.status.code(), Some(7));
    assert!(fixture.tui_marker.is_file(), "the TUI never ran");
    assert!(wait_for_file(&fixture.worker_pid), "worker never started");
    assert_eq!(
        fs::read_to_string(&fixture.tui_env).expect("read TUI environment"),
        "from-dotenv|from-shell|fresh-clone-key\n"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Starting agentos-tui"), "{stdout}");

    drop(engine);
    fixture.cleanup();
}

#[test]
fn up_reports_the_install_hint_when_the_engine_binary_is_missing() {
    let fixture = Fixture::new("missing-engine");
    fs::remove_file(fixture.bin.join("iii")).expect("remove fake engine");

    let cli = PathBuf::from(env!("CARGO_BIN_EXE_agentos"));
    let output = fixture.up(&cli, &["--no-tui"], &fixture.bin.display().to_string());
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("install-iii.sh"), "{stderr}");
    assert!(!fixture.worker_pid.exists(), "workers started anyway");

    fixture.cleanup();
}

#[test]
fn up_gives_up_within_the_health_timeout_when_the_engine_never_listens() {
    let _guard = engine_port_lock();
    let fixture = Fixture::new("health-timeout");
    match TcpListener::bind(("127.0.0.1", ENGINE_PORT)) {
        Ok(listener) => drop(listener),
        Err(_) => {
            eprintln!("skipped: port {ENGINE_PORT} is already in use by another engine");
            fixture.cleanup();
            return;
        }
    }

    let cli = PathBuf::from(env!("CARGO_BIN_EXE_agentos"));
    let started = Instant::now();
    let output = fixture.up(
        &cli,
        &["--no-tui", "--timeout", "2"],
        &fixture.path_with_engine(),
    );
    let elapsed = started.elapsed();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("did not accept connections on 127.0.0.1:49134"),
        "{stderr}"
    );
    assert!(stderr.contains("engine.log"), "{stderr}");
    assert!(
        elapsed < Duration::from_secs(20),
        "unbounded wait: {elapsed:?}"
    );
    assert!(
        fixture.engine_marker.is_file(),
        "the engine was never started"
    );
    assert!(
        !fixture.worker_pid.exists(),
        "workers started after a dead engine"
    );

    fixture.cleanup();
}

#[test]
fn up_fails_when_a_started_worker_dies_before_reaching_the_bus() {
    let _guard = engine_port_lock();
    let fixture = Fixture::new("worker-dies");
    let Ok(engine) = TcpListener::bind(("127.0.0.1", ENGINE_PORT)) else {
        eprintln!("skipped: port {ENGINE_PORT} is already in use by another engine");
        fixture.cleanup();
        return;
    };
    fixture.with_failing_worker();

    let cli = fixture.with_tui(0);
    let started = Instant::now();
    let output = fixture.up(&cli, &["--timeout", "3"], &fixture.path_with_engine());
    let elapsed = started.elapsed();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("exited right after starting"), "{stderr}");
    assert!(stderr.contains("echo"), "{stderr}");
    assert!(stderr.contains("workers.log"), "{stderr}");
    assert!(
        elapsed < Duration::from_secs(20),
        "unbounded wait: {elapsed:?}"
    );
    assert!(
        !fixture.tui_marker.exists(),
        "the TUI ran on an unready stack"
    );

    drop(engine);
    fixture.cleanup();
}

/// `.env.example` ships `AGENTOS_API_KEY=` empty and `crates/http-adapter`
/// refuses to register protected routes without it, so a clean machine loses
/// almost every worker. `up` must close that gap by itself, before it touches
/// the stack, whatever the engine does afterwards. This test is deliberately
/// independent of the shared engine port.
#[test]
fn up_writes_the_machine_api_key_before_starting_anything() {
    let fixture = Fixture::new("generate-api-key");
    fixture.with_empty_api_key();

    let cli = PathBuf::from(env!("CARGO_BIN_EXE_agentos"));
    let output = fixture.up(
        &cli,
        &["--no-tui", "--timeout", "2"],
        &fixture.path_with_engine(),
    );

    let key = fixture
        .dotenv_value("AGENTOS_API_KEY")
        .expect("AGENTOS_API_KEY assignment");
    assert_eq!(key.len(), 64, "expected 32 bytes of hex, got {key:?}");
    assert!(
        key.chars().all(|c| c.is_ascii_hexdigit()),
        "not a hex key: {key}"
    );
    assert_eq!(fixture.dotenv_mode(), 0o600, "{}", fixture.dotenv());
    assert!(
        fixture.dotenv().contains("DOTENV_ONLY=from-dotenv"),
        "the operator's other values must survive: {}",
        fixture.dotenv()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("generated a new 32-byte AGENTOS_API_KEY"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&fixture.runtime.join(".env").display().to_string()),
        "up must print where the key went: {stdout}"
    );

    fixture.cleanup();
}

/// The generated key is worthless unless the processes that need it receive it.
#[test]
fn up_hands_the_generated_api_key_to_the_launched_processes() {
    let _guard = engine_port_lock();
    let fixture = Fixture::new("propagate-api-key");
    let Ok(engine) = TcpListener::bind(("127.0.0.1", ENGINE_PORT)) else {
        eprintln!("skipped: port {ENGINE_PORT} is already in use by another engine");
        fixture.cleanup();
        return;
    };
    fixture.with_empty_api_key();

    let cli = fixture.with_tui(0);
    let output = fixture.up(&cli, &[], &fixture.path_with_engine());
    assert!(
        output.status.success(),
        "up failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let key = fixture
        .dotenv_value("AGENTOS_API_KEY")
        .expect("AGENTOS_API_KEY assignment");
    assert_eq!(
        fs::read_to_string(&fixture.tui_env).expect("read TUI environment"),
        format!("from-dotenv|from-shell|{key}\n")
    );

    drop(engine);
    fixture.cleanup();
}

#[test]
fn up_never_overwrites_an_existing_api_key() {
    let fixture = Fixture::new("keep-api-key");
    let before = fixture.dotenv();

    let cli = PathBuf::from(env!("CARGO_BIN_EXE_agentos"));
    let output = fixture.up(
        &cli,
        &["--no-tui", "--timeout", "2"],
        &fixture.path_with_engine(),
    );
    assert_eq!(fixture.dotenv(), before, "up rewrote an operator secret");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("AGENTOS_API_KEY already set in"),
        "{stdout}"
    );

    fixture.cleanup();
}

/// `start` used to pass an empty environment to the engine and the workers,
/// so a mode-600 `.env` honoured by `up` was silently ignored here.
#[test]
fn start_loads_the_active_dotenv_and_generates_the_api_key() {
    let fixture = Fixture::new("start-dotenv");
    fixture.with_empty_api_key();

    let cli = PathBuf::from(env!("CARGO_BIN_EXE_agentos"));
    let mut child = fixture.start(&cli, &fixture.path_with_engine());
    let started = wait_for_file(&fixture.worker_env);
    let _ = child.kill();
    let _ = child.wait();

    assert!(started, "start never launched the worker");
    assert_eq!(
        fs::read_to_string(&fixture.worker_env).expect("read worker environment"),
        "from-dotenv|from-shell|echo\n",
        "start must load the same dotenv as up"
    );
    let key = fixture
        .dotenv_value("AGENTOS_API_KEY")
        .expect("AGENTOS_API_KEY assignment");
    assert_eq!(key.len(), 64, "start must generate the machine key too");
    assert_eq!(fixture.dotenv_mode(), 0o600);

    fixture.cleanup();
}

/// Bus RBAC is fail-closed: iii 0.22.1 calls the auth function for every bus
/// connection, so the daemon has to be listening before the engine starts.
#[test]
fn up_starts_the_bus_auth_daemon_with_the_generated_key() {
    let fixture = Fixture::new("bus-auth");
    if Command::new("python3")
        .arg("--version")
        .output()
        .map(|output| !output.status.success())
        .unwrap_or(true)
    {
        eprintln!("skipped: python3 is needed to hold the daemon port");
        fixture.cleanup();
        return;
    }
    fixture.with_empty_api_key();
    let addr = free_loopback_addr();
    let addr = addr.as_str();
    fixture.with_bus_auth(addr);

    // Run the copy inside the fixture: `up` looks for the daemon next to the
    // CLI first, and `cargo test --workspace` puts a real one beside the
    // workspace binary.
    let cli = fixture.cli();
    let output = Command::new(&cli)
        .args(["up", "--no-tui", "--timeout", "5"])
        .env("PATH", fixture.path_with_engine())
        .env("HOME", &fixture.home)
        .env("AGENTOS_HOME", &fixture.home)
        .env("AGENTOS_CONFIG", fixture.runtime.join("config.yaml"))
        .env("AGENTOS_BUS_AUTH_ADDR", addr)
        .env_remove("AGENTOS_API_KEY")
        .current_dir(&fixture.root)
        .output()
        .expect("run agentos up");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        wait_for_file(&fixture.bus_auth_env),
        "the daemon never started: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = fs::read_to_string(&fixture.bus_auth_env).expect("read daemon environment");
    let (listen, key) = recorded
        .trim()
        .split_once('|')
        .expect("daemon recorded --listen and the key");
    assert_eq!(listen, format!("--listen={addr}"));
    // It refuses to start without the key, so `up` must generate it first.
    assert_eq!(
        key,
        fixture
            .dotenv_value("AGENTOS_API_KEY")
            .expect("generated key")
            .as_str()
    );
    assert!(stdout.contains(&format!("listening on {addr}")), "{stdout}");

    fixture.cleanup();
}

/// An armed config with no daemon binary cannot boot: the engine refuses every
/// bus connection. Saying so beats 62 workers failing for no stated reason.
#[test]
fn up_refuses_an_armed_gate_without_the_daemon_binary() {
    let fixture = Fixture::new("bus-auth-missing");
    let addr = free_loopback_addr();
    let addr = addr.as_str();
    fixture.with_bus_auth(addr);
    fs::remove_file(fixture.release.join("agentos-bus-authd")).expect("remove fake daemon");

    // The fixture CLI copy has no daemon beside it either, so "not built"
    // really means not built.
    let cli = fixture.cli();
    let output = Command::new(&cli)
        .args(["up", "--no-tui", "--timeout", "2"])
        .env("PATH", fixture.path_with_engine())
        .env("HOME", &fixture.home)
        .env("AGENTOS_HOME", &fixture.home)
        .env("AGENTOS_CONFIG", fixture.runtime.join("config.yaml"))
        .env("AGENTOS_BUS_AUTH_ADDR", addr)
        .env_remove("AGENTOS_API_KEY")
        .current_dir(&fixture.root)
        .output()
        .expect("run agentos up");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("agentos-bus-authd"), "{stderr}");
    assert!(stderr.contains("refuses every bus connection"), "{stderr}");
    assert!(!fixture.worker_pid.exists(), "workers started anyway");

    fixture.cleanup();
}
