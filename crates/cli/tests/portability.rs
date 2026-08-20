#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn temporary_directory(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "agentos-cli-{label}-{}-{stamp}",
        std::process::id()
    ));
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

fn wait_for_file(path: &Path) {
    for _ in 0..100 {
        if path.is_file() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {}", path.display());
}

fn assert_process_gone(pid_path: &Path) {
    wait_for_file(pid_path);
    let pid = fs::read_to_string(pid_path).expect("read child pid");
    for _ in 0..100 {
        let status = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .expect("check child process");
        if !status.success() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("child process {pid} survived cleanup");
}

fn stop_cli(child: &mut Child) {
    let status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT to agentos");
    assert!(status.success(), "failed to interrupt agentos");
    let status = child.wait().expect("wait for agentos");
    assert!(
        status.success() || status.signal() == Some(2),
        "agentos exited unsuccessfully: {status}"
    );
}

fn run_start_with_relative_config(config_override: &str) {
    let root = temporary_directory("start");
    let caller = root.join("caller");
    let runtime = if config_override.contains('/') {
        caller.join("nested")
    } else {
        caller.clone()
    };
    let state = caller.join("state");
    let bin = root.join("bin");
    fs::create_dir_all(runtime.join("workers/echo")).expect("create worker directory");
    fs::create_dir_all(runtime.join("target/release")).expect("create worker binary directory");
    fs::create_dir_all(&bin).expect("create fake engine directory");

    let config_path = runtime.join("runtime.yaml");
    fs::write(&config_path, "workers: []\n").expect("write runtime config");
    fs::write(
        runtime.join("workers/echo/iii.worker.yaml"),
        "iii: v1\nname: echo\nruntime: rust\nscripts:\n  start: echo\n",
    )
    .expect("write worker manifest");

    let engine_pid = caller.join("engine.pid");
    let engine_cwd = caller.join("engine.cwd");
    let worker_pid = caller.join("worker.pid");
    let worker_cwd = caller.join("worker.cwd");
    write_executable(
        &bin.join("iii"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" > '{}'\nprintf '%s\\n' \"$$\" > '{}'\ntrap 'exit 0' INT TERM\nwhile :; do sleep 1; done\n",
            engine_cwd.display(),
            engine_pid.display()
        ),
    );
    write_executable(
        &runtime.join("target/release/agentos-echo"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" > '{}'\nprintf '%s\\n' \"$$\" > '{}'\ntrap 'exit 0' INT TERM\nwhile :; do sleep 1; done\n",
            worker_cwd.display(),
            worker_pid.display()
        ),
    );

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let path = format!("{}:{}", bin.display(), old_path.to_string_lossy());
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentos"))
        .arg("start")
        .current_dir(&caller)
        .env("PATH", path)
        .env("AGENTOS_HOME", "state")
        .env("AGENTOS_CONFIG", config_override)
        .env("AGENTOS_API_URL", "http://127.0.0.1:1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agentos start");

    wait_for_file(&engine_cwd);
    wait_for_file(&worker_cwd);
    assert_eq!(
        fs::read_to_string(&engine_cwd)
            .expect("read engine cwd")
            .trim(),
        runtime.to_string_lossy()
    );
    assert_eq!(
        fs::read_to_string(&worker_cwd)
            .expect("read worker cwd")
            .trim(),
        runtime.to_string_lossy()
    );
    assert!(state.is_dir(), "relative AGENTOS_HOME was not created");
    thread::sleep(Duration::from_secs(4));

    stop_cli(&mut child);
    assert_process_gone(&engine_pid);
    assert_process_gone(&worker_pid);
    fs::remove_dir_all(root).expect("remove temporary directory");
}

#[test]
fn start_normalizes_one_component_relative_config_and_home() {
    run_start_with_relative_config("runtime.yaml");
}

#[test]
fn start_normalizes_nested_relative_config_and_home() {
    run_start_with_relative_config("nested/runtime.yaml");
}

#[test]
fn start_uses_relative_home_for_installed_runtime() {
    let root = temporary_directory("installed-home");
    let caller = root.join("caller");
    let home = caller.join("installed");
    let runtime = home.join("runtime");
    let bin = root.join("bin");
    fs::create_dir_all(runtime.join("workers/echo")).expect("create worker directory");
    fs::create_dir_all(runtime.join("target/release")).expect("create worker binary directory");
    fs::create_dir_all(&bin).expect("create fake engine directory");
    fs::write(runtime.join("config.yaml"), "workers: []\n").expect("write runtime config");
    fs::write(
        runtime.join("workers/echo/iii.worker.yaml"),
        "iii: v1\nname: echo\nruntime: rust\nscripts:\n  start: echo\n",
    )
    .expect("write worker manifest");

    let engine_cwd = caller.join("engine.cwd");
    let engine_pid = caller.join("engine.pid");
    let worker_cwd = caller.join("worker.cwd");
    let worker_pid = caller.join("worker.pid");
    write_executable(
        &bin.join("iii"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" > '{}'\nprintf '%s\\n' \"$$\" > '{}'\ntrap 'exit 0' INT TERM\nwhile :; do sleep 1; done\n",
            engine_cwd.display(),
            engine_pid.display()
        ),
    );
    write_executable(
        &runtime.join("target/release/agentos-echo"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" > '{}'\nprintf '%s\\n' \"$$\" > '{}'\ntrap 'exit 0' INT TERM\nwhile :; do sleep 1; done\n",
            worker_cwd.display(),
            worker_pid.display()
        ),
    );

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let path = format!("{}:{}", bin.display(), old_path.to_string_lossy());
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentos"))
        .arg("start")
        .current_dir(&caller)
        .env("PATH", path)
        .env("AGENTOS_HOME", "installed")
        .env_remove("AGENTOS_CONFIG")
        .env("AGENTOS_API_URL", "http://127.0.0.1:1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agentos start");

    wait_for_file(&engine_cwd);
    wait_for_file(&worker_cwd);
    assert_eq!(
        fs::read_to_string(&engine_cwd)
            .expect("read engine cwd")
            .trim(),
        runtime.to_string_lossy()
    );
    assert_eq!(
        fs::read_to_string(&worker_cwd)
            .expect("read worker cwd")
            .trim(),
        runtime.to_string_lossy()
    );
    assert!(
        home.join("logs").is_dir(),
        "relative AGENTOS_HOME was not used"
    );
    thread::sleep(Duration::from_secs(4));

    stop_cli(&mut child);
    assert_process_gone(&engine_pid);
    assert_process_gone(&worker_pid);
    fs::remove_dir_all(root).expect("remove temporary directory");
}

#[test]
fn start_fails_closed_when_engine_exits_before_workers() {
    let root = temporary_directory("engine-exit");
    let runtime = root.join("runtime");
    let bin = root.join("bin");
    fs::create_dir_all(runtime.join("workers/echo")).expect("create worker directory");
    fs::create_dir_all(&bin).expect("create fake engine directory");
    fs::write(runtime.join("config.yaml"), "workers: []\n").expect("write runtime config");
    fs::write(
        runtime.join("workers/echo/iii.worker.yaml"),
        "iii: v1\nname: echo\nruntime: rust\nscripts:\n  start: echo\n",
    )
    .expect("write worker manifest");
    write_executable(&bin.join("iii"), "#!/bin/sh\nexit 0\n");

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let path = format!("{}:{}", bin.display(), old_path.to_string_lossy());
    let output = Command::new(env!("CARGO_BIN_EXE_agentos"))
        .arg("start")
        .current_dir(&root)
        .env("PATH", path)
        .env("AGENTOS_HOME", root.join("home"))
        .env("AGENTOS_CONFIG", runtime.join("config.yaml"))
        .output()
        .expect("run agentos start");
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("iii-engine running"));
    fs::remove_dir_all(root).expect("remove temporary directory");
}

#[test]
fn start_fails_closed_when_worker_launch_fails() {
    let root = temporary_directory("worker-fail");
    let runtime = root.join("runtime");
    let bin = root.join("bin");
    fs::create_dir_all(runtime.join("workers/echo")).expect("create worker directory");
    fs::create_dir_all(runtime.join("target/release")).expect("create worker binary directory");
    fs::create_dir_all(&bin).expect("create fake engine directory");
    fs::write(runtime.join("config.yaml"), "workers: []\n").expect("write runtime config");
    fs::write(
        runtime.join("workers/echo/iii.worker.yaml"),
        "iii: v1\nname: echo\nruntime: rust\nscripts:\n  start: echo\n",
    )
    .expect("write worker manifest");
    let engine_pid = root.join("engine.pid");
    write_executable(
        &bin.join("iii"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{}'\ntrap 'exit 0' INT TERM\nwhile :; do sleep 1; done\n",
            engine_pid.display()
        ),
    );
    write_executable(
        &runtime.join("target/release/agentos-echo"),
        "#!/bin/sh\nexit 0\n",
    );

    let old_path = std::env::var_os("PATH").unwrap_or_default();
    let path = format!("{}:{}", bin.display(), old_path.to_string_lossy());
    let output = Command::new(env!("CARGO_BIN_EXE_agentos"))
        .arg("start")
        .current_dir(&root)
        .env("PATH", path)
        .env("AGENTOS_HOME", root.join("home"))
        .env("AGENTOS_CONFIG", runtime.join("config.yaml"))
        .output()
        .expect("run agentos start");
    assert!(!output.status.success());
    assert_process_gone(&engine_pid);
    fs::remove_dir_all(root).expect("remove temporary directory");
}

fn assert_doctor_workers_failed(runtime: &Path, home: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_agentos"))
        .args(["doctor", "--json"])
        .current_dir(runtime)
        .env("AGENTOS_HOME", home)
        .env("AGENTOS_CONFIG", runtime.join("config.yaml"))
        .env("AGENTOS_API_URL", "http://127.0.0.1:1")
        .output()
        .expect("run agentos doctor");
    assert!(output.status.success());
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse doctor report");
    let workers = report["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|check| check["check"] == "Workers"))
        .expect("find Workers doctor check");
    assert_eq!(workers["passed"], false);
}

#[test]
fn doctor_reports_missing_workers_directory() {
    let root = temporary_directory("doctor-missing-workers");
    let runtime = root.join("runtime");
    let home = root.join("home");
    fs::create_dir_all(&runtime).expect("create runtime directory");
    fs::write(runtime.join("config.yaml"), "workers: []\n").expect("write runtime config");

    assert_doctor_workers_failed(&runtime, &home);
    fs::remove_dir_all(root).expect("remove temporary doctor directory");
}

#[cfg(unix)]
#[test]
fn doctor_reports_unreadable_workers_directory() {
    let root = temporary_directory("doctor-unreadable-workers");
    let runtime = root.join("runtime");
    let home = root.join("home");
    let workers = runtime.join("workers");
    fs::create_dir_all(&workers).expect("create workers directory");
    fs::write(runtime.join("config.yaml"), "workers: []\n").expect("write runtime config");
    let mut permissions = fs::metadata(&workers)
        .expect("read workers directory metadata")
        .permissions();
    permissions.set_mode(0o0);
    fs::set_permissions(&workers, permissions).expect("make workers unreadable");

    assert_doctor_workers_failed(&runtime, &home);

    let mut permissions = fs::metadata(&workers)
        .expect("read workers directory metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&workers, permissions).expect("restore workers permissions");
    fs::remove_dir_all(root).expect("remove temporary doctor directory");
}
