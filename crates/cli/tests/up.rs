//! End-to-end behaviour of `agentos up` against fake engine, worker, and TUI
//! binaries. The engine port is shared machine state, so every test that
//! depends on it takes `ENGINE_PORT_LOCK`.

#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
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

/// A minimal stand-in for the engine API. It reports one connected worker once
/// the fake worker has written its pid file, so `up` can observe the same
/// "spawned, then actually on the bus" transition a real stack goes through.
fn serve_worker_health(worker_pid: PathBuf) -> String {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fake engine api");
    let port = listener
        .local_addr()
        .expect("fake engine api address")
        .port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let workers = u8::from(worker_pid.is_file());
            let body = format!("{{\"workers\":{workers}}}");
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
            let _ = stream.flush();
        }
    });
    format!("http://127.0.0.1:{port}")
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
    release: PathBuf,
    api_base: String,
    tui_marker: PathBuf,
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
            runtime.join("workers/echo/iii.worker.yaml"),
            "iii: v1\nname: echo\nruntime: rust\nscripts:\n  start: echo\n",
        )
        .expect("write worker manifest");

        let engine_marker = root.join("engine.started");
        let worker_pid = root.join("worker.pid");
        let tui_marker = root.join("tui.started");
        write_executable(
            &bin.join("iii"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'iii 0.22.1'; exit 0; fi\necho started > {}\nexec sleep 60\n",
                engine_marker.display()
            ),
        );
        write_executable(
            &release.join("agentos-echo"),
            &format!(
                "#!/bin/sh\necho $$ > {}\nexec sleep 60\n",
                worker_pid.display()
            ),
        );

        let api_base = serve_worker_health(worker_pid.clone());

        Self {
            root,
            home,
            runtime,
            bin,
            engine_marker,
            worker_pid,
            release,
            api_base,
            tui_marker,
        }
    }

    /// Installs a fake TUI beside a copy of the CLI, mirroring an installed
    /// release layout where `up` finds the TUI next to itself.
    fn with_tui(&self, exit_code: i32) -> PathBuf {
        let cli = self.bin.join("agentos");
        fs::copy(env!("CARGO_BIN_EXE_agentos"), &cli).expect("copy agentos binary");
        let mut permissions = fs::metadata(&cli).expect("read cli metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&cli, permissions).expect("make cli executable");
        write_executable(
            &self.bin.join("agentos-tui"),
            &format!(
                "#!/bin/sh\necho started > {}\nexit {exit_code}\n",
                self.tui_marker.display()
            ),
        );
        cli
    }

    /// Replaces the worker with one that dies straight after being spawned.
    fn with_failing_worker(&self) {
        write_executable(&self.release.join("agentos-echo"), "#!/bin/sh\nexit 3\n");
    }

    fn up(&self, program: &Path, arguments: &[&str], path: &str) -> Output {
        Command::new(program)
            .arg("up")
            .args(arguments)
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("AGENTOS_HOME", &self.home)
            .env("AGENTOS_CONFIG", self.runtime.join("config.yaml"))
            .env("AGENTOS_API_URL", &self.api_base)
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

    fn cleanup(self) {
        self.stop_worker();
        fs::remove_dir_all(&self.root).expect("remove temporary directory");
    }
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
