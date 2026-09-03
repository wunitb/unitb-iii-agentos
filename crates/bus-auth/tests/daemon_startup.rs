//! The daemon's startup refusals, driven through the real binary.
//!
//! These spawn `agentos-bus-authd` and assert it EXITS. Nothing binds a port and
//! nothing has to be running: every case here fails before the listener is
//! created, which is the point — a daemon that cannot check the config it is
//! gating must not reach the point of accepting connections.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/bus-auth is two levels below the repository root")
        .to_path_buf()
}

fn daemon() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentos-bus-authd"));
    command.env("AGENTOS_API_KEY", "startup-test-key");
    command.env_remove("AGENTOS_CONFIG");
    // A directory with no config.yaml, so the cwd probe cannot rescue a case.
    command.current_dir(std::env::temp_dir());
    command
}

fn refusal(command: &mut Command) -> String {
    let output = command.output().expect("run agentos-bus-authd");
    assert!(
        !output.status.success(),
        "the daemon started anyway: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The safety net must not be disableable by a typo in the path that names it.
#[test]
fn a_named_config_that_cannot_be_read_stops_the_daemon() {
    let missing = std::env::temp_dir().join("agentos-bus-auth-typo-does-not-exist.yaml");
    let stderr = refusal(
        daemon()
            .arg("--listen=127.0.0.1:0")
            .arg(format!("--config={}", missing.display())),
    );
    assert!(stderr.contains("cannot read"), "{stderr}");
    assert!(
        stderr.contains("silently disable the check"),
        "the message has to say why a missing path is fatal: {stderr}"
    );

    let stderr = refusal(
        daemon()
            .arg("--listen=127.0.0.1:0")
            .env("AGENTOS_CONFIG", &missing),
    );
    assert!(
        stderr.contains("cannot read"),
        "AGENTOS_CONFIG is just as much a statement of intent as --config=: {stderr}"
    );
}

/// The overlay this repository ships, with one hook id typo'd — the exact edit
/// the engine accepts in silence.
#[test]
fn an_armed_config_the_daemon_cannot_honour_stops_it() {
    let overlay = std::fs::read_to_string(repository_root().join("bus-rbac.overlay.yaml"))
        .expect("read bus-rbac.overlay.yaml");
    let broken = std::env::temp_dir().join("agentos-bus-auth-startup-typo.yaml");
    std::fs::write(
        &broken,
        overlay.replace(
            "auth_function_id: agentos::bus_auth",
            "auth_function_idd: agentos::bus_auth",
        ),
    )
    .expect("write the typo'd config");

    let stderr = refusal(
        daemon()
            .arg("--listen=127.0.0.1:0")
            .arg(format!("--config={}", broken.display())),
    );
    assert!(stderr.contains("the engine would NOT tell you"), "{stderr}");
    assert!(stderr.contains("rbac.auth_function_idd"), "{stderr}");

    std::fs::remove_file(&broken).ok();
}

/// No key, no daemon: everything would land in the untrusted tier while looking
/// armed.
#[test]
fn a_missing_credential_stops_the_daemon() {
    let mut command = daemon();
    command.env_remove("AGENTOS_API_KEY");
    let stderr = refusal(command.arg("--listen=127.0.0.1:0"));
    assert!(stderr.contains("AGENTOS_API_KEY"), "{stderr}");
}
