use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const RECORD_VERSION: u32 = 1;
const RECORD_RELATIVE_PATH: &str = "run/owned-processes.json";

#[derive(Debug, Clone)]
pub(crate) struct OwnedCandidate {
    role: String,
    pid: u32,
    start_token: Option<u64>,
}

impl OwnedCandidate {
    pub(crate) fn spawned(role: impl Into<String>, pid: u32) -> Self {
        #[cfg(target_os = "linux")]
        let start_token = process_identity(pid)
            .ok()
            .map(|identity| identity.start_token);
        #[cfg(not(target_os = "linux"))]
        let start_token = None;
        Self {
            role: role.into(),
            pid,
            // The Child handle is the launch authority. Capture the kernel birth
            // token immediately; a process that exits too quickly is handled by
            // readiness/rollback and is never persisted.
            start_token,
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn finalize(&self) -> Result<OwnedProcess> {
        let expected_start = self
            .start_token
            .context("spawned process exited before its ownership identity could be captured")?;
        let observed = process_identity(self.pid)?;
        if observed.zombie || observed.start_token != expected_start {
            anyhow::bail!(
                "spawned pid {} changed identity before persistence",
                self.pid
            );
        }
        Ok(OwnedProcess {
            role: self.role.clone(),
            pid: self.pid,
            process_group: self.pid,
            start_token: expected_start,
            executable: observed.executable,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn finalize(&self) -> Result<OwnedProcess> {
        ensure_supported()?;
        unreachable!()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnedProcess {
    pub(crate) role: String,
    pub(crate) pid: u32,
    pub(crate) process_group: u32,
    pub(crate) start_token: u64,
    /// Kernel executable (`/proc/<pid>/exe`), which is the interpreter for a script.
    pub(crate) executable: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnershipRecord {
    version: u32,
    processes: Vec<OwnedProcess>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    NothingRecorded,
    Stopped(usize),
}

pub(crate) fn ensure_supported() -> Result<()> {
    #[cfg(target_os = "linux")]
    return Ok(());
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!(
        "detached AgentOS lifecycle ownership is supported only on Linux; use `agentos start` for foreground lifecycle on this platform"
    );
}

#[derive(Debug)]
struct ProcessIdentity {
    start_token: u64,
    executable: PathBuf,
    zombie: bool,
}

#[cfg(target_os = "linux")]
fn process_identity(pid: u32) -> Result<ProcessIdentity> {
    let stat_path = format!("/proc/{pid}/stat");
    let stat = fs::read_to_string(&stat_path)
        .with_context(|| format!("Cannot read process identity {stat_path}"))?;
    let (_, fields) = stat
        .rsplit_once(") ")
        .with_context(|| format!("Malformed process identity {stat_path}"))?;
    let fields = fields.split_whitespace().collect::<Vec<_>>();
    let state = fields
        .first()
        .copied()
        .context("Process stat has no state")?;
    let start_token = fields
        .get(19)
        .context("Process stat has no start token")?
        .parse::<u64>()
        .context("Process start token is not numeric")?;
    let zombie = state == "Z";
    let executable = if zombie {
        PathBuf::new()
    } else {
        fs::canonicalize(format!("/proc/{pid}/exe"))
            .with_context(|| format!("Cannot resolve executable for pid {pid}"))?
    };
    Ok(ProcessIdentity {
        start_token,
        executable,
        zombie,
    })
}

fn record_path(agentos_home: &Path) -> PathBuf {
    agentos_home.join(RECORD_RELATIVE_PATH)
}

#[cfg(target_os = "linux")]
fn effective_uid() -> Result<u32> {
    let status =
        fs::read_to_string("/proc/self/status").context("Cannot read /proc/self/status")?;
    let line = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .context("/proc/self/status has no Uid")?;
    line.split_whitespace()
        .nth(2)
        .context("/proc/self/status has no effective uid")?
        .parse()
        .context("effective uid is not numeric")
}

#[cfg(target_os = "linux")]
fn validate_record_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Cannot inspect lifecycle record {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        anyhow::bail!(
            "Lifecycle record {} must be a regular file, not a symlink",
            path.display()
        );
    }
    if metadata.uid() != effective_uid()? || metadata.mode() & 0o077 != 0 {
        anyhow::bail!(
            "Lifecycle record {} must be owned by the current user with mode 0600",
            path.display()
        );
    }
    Ok(())
}

fn read_record(agentos_home: &Path) -> Result<Option<OwnershipRecord>> {
    let path = record_path(agentos_home);
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Cannot inspect {}", path.display()));
        }
    }
    #[cfg(target_os = "linux")]
    validate_record_file(&path)?;
    let record: OwnershipRecord = serde_json::from_slice(
        &fs::read(&path).with_context(|| format!("Cannot read {}", path.display()))?,
    )
    .with_context(|| format!("Invalid lifecycle record {}", path.display()))?;
    if record.version != RECORD_VERSION {
        anyhow::bail!("Unsupported lifecycle record version {}", record.version);
    }
    if record.processes.len() > 128 {
        anyhow::bail!("Lifecycle record contains too many processes");
    }
    let mut pids = BTreeSet::new();
    for process in &record.processes {
        if process.pid == 0
            || process.process_group != process.pid
            || !matches!(process.role.as_str(), "worker" | "engine" | "bus-auth")
            || !pids.insert(process.pid)
        {
            anyhow::bail!("Invalid role, process group, or duplicate pid in lifecycle record");
        }
    }
    Ok(Some(record))
}

pub(crate) fn persist_owned(agentos_home: &Path, mut started: Vec<OwnedProcess>) -> Result<()> {
    ensure_supported()?;
    if started.is_empty() {
        return Ok(());
    }
    if let Some(existing) = read_record(agentos_home)? {
        for process in existing.processes {
            #[cfg(target_os = "linux")]
            match process_identity(process.pid) {
                Err(_) => {} // a fully exited prior process is stale, not authority
                Ok(identity) if identity.zombie => {}
                Ok(identity)
                    if identity.start_token == process.start_token
                        && identity.executable == process.executable =>
                {
                    started.push(process);
                }
                Ok(_) => anyhow::bail!(
                    "refusing to replace lifecycle record: pid {} no longer matches its recorded identity",
                    process.pid
                ),
            }
        }
    }
    let mut pids = BTreeSet::new();
    started.retain(|process| pids.insert(process.pid));
    let record = OwnershipRecord {
        version: RECORD_VERSION,
        processes: started,
    };
    let path = record_path(agentos_home);
    let parent = path.parent().context("Lifecycle record has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("Cannot create {}", parent.display()))?;
    if fs::symlink_metadata(&path).is_ok() {
        #[cfg(target_os = "linux")]
        validate_record_file(&path)?;
    }
    let temp = parent.join(format!(
        ".owned-processes.{}.{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    #[cfg(unix)]
    let mut output = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
    }?;
    #[cfg(not(unix))]
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let write_result = (|| -> Result<()> {
        serde_json::to_writer_pretty(&mut output, &record)?;
        output.write_all(b"\n")?;
        output.sync_all()?;
        fs::rename(&temp, &path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    write_result.with_context(|| format!("Cannot persist lifecycle record {}", path.display()))
}

#[cfg(target_os = "linux")]
fn verify_identity(process: &OwnedProcess) -> Result<()> {
    let identity = process_identity(process.pid)?;
    if identity.zombie
        || identity.start_token != process.start_token
        || identity.executable != process.executable
    {
        anyhow::bail!(
            "refusing pid {} for role {}: recorded process identity no longer matches",
            process.pid,
            process.role
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn leader_is_running(process: &OwnedProcess) -> Result<bool> {
    match process_identity(process.pid) {
        Ok(identity) if identity.zombie => Ok(false),
        Ok(identity)
            if identity.start_token == process.start_token
                && identity.executable == process.executable =>
        {
            Ok(true)
        }
        Ok(_) => anyhow::bail!(
            "refusing pid {} for role {}: recorded process identity no longer matches",
            process.pid,
            process.role
        ),
        Err(_error) if !Path::new(&format!("/proc/{}/exe", process.pid)).exists() => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

#[cfg(target_os = "linux")]
fn signal_verified_group(process: &OwnedProcess, signal: i32) -> Result<()> {
    verify_identity(process)?;
    let group = i32::try_from(process.process_group).context("process group does not fit i32")?;
    // Linux/POSIX: a negative pid addresses exactly that process group.
    if unsafe { kill(-group, signal) } != 0 && leader_is_running(process)? {
        return Err(std::io::Error::last_os_error()).context(format!(
            "Cannot signal owned process group {} with signal {signal}",
            process.process_group
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn live_group_members(process_group: u32) -> Result<Vec<u32>> {
    let mut members = Vec::new();
    for entry in fs::read_dir("/proc").context("Cannot enumerate /proc for owned descendants")? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let stat_path = entry.path().join("stat");
        let stat_bytes = match fs::read(&stat_path) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Cannot inspect process group via {}", stat_path.display())
                });
            }
        };
        let stat = String::from_utf8_lossy(&stat_bytes);
        let Some((_, fields)) = stat.rsplit_once(") ") else {
            anyhow::bail!("Malformed process identity {}", stat_path.display());
        };
        let fields = fields.split_whitespace().collect::<Vec<_>>();
        let state = fields
            .first()
            .copied()
            .context("Process stat has no state")?;
        let group = fields
            .get(2)
            .context("Process stat has no process group")?
            .parse::<u32>()
            .context("Process group is not numeric")?;
        if group == process_group && state != "Z" {
            members.push(pid);
        }
    }
    members.sort_unstable();
    Ok(members)
}

pub(crate) fn stop_owned(agentos_home: &Path, grace: Duration) -> Result<StopOutcome> {
    ensure_supported()?;
    let Some(mut record) = read_record(agentos_home)? else {
        return Ok(StopOutcome::NothingRecorded);
    };
    // Validate the complete record before signalling anything. A single stale or
    // tampered entry makes the whole operation non-destructive.
    #[cfg(target_os = "linux")]
    for process in &record.processes {
        verify_identity(process)?;
    }
    record
        .processes
        .sort_by_key(|process| match process.role.as_str() {
            "worker" => 0,
            "engine" => 1,
            "bus-auth" => 2,
            _ => 3,
        });
    let count = record.processes.len();
    #[cfg(target_os = "linux")]
    {
        // SIGTERM every verified group first, then give all services the same
        // bounded grace window for state flushes and in-flight work.
        for process in &record.processes {
            signal_verified_group(process, 15)?;
        }
        let deadline = Instant::now() + grace;
        loop {
            let mut running = false;
            for process in &record.processes {
                running |= leader_is_running(process)?;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !running || remaining.is_zero() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20).min(remaining));
        }

        // Escalation is authorized only while the original leader identity is
        // still present. A group whose leader exited is never blind-signalled.
        for process in &record.processes {
            if leader_is_running(process)? {
                signal_verified_group(process, 9)?;
            }
        }
        let kill_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let mut running = false;
            for process in &record.processes {
                running |= leader_is_running(process)?;
            }
            let remaining = kill_deadline.saturating_duration_since(Instant::now());
            if !running || remaining.is_zero() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20).min(remaining));
        }
        for process in &record.processes {
            if leader_is_running(process)? {
                anyhow::bail!(
                    "owned process {} did not stop; lifecycle record preserved",
                    process.pid
                );
            }
        }
        for process in &record.processes {
            let survivors = live_group_members(process.process_group)?;
            if !survivors.is_empty() {
                anyhow::bail!(
                    "leader {} exited but descendant cleanup after leader exit is not guaranteed; surviving pids in owned process group {}: {:?}; lifecycle record preserved",
                    process.pid,
                    process.process_group,
                    survivors
                );
            }
        }
    }
    fs::remove_file(record_path(agentos_home))
        .context("Cannot remove lifecycle record after stop")?;
    Ok(StopOutcome::Stopped(count))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agentos-lifecycle-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn spawn_sleep() -> std::process::Child {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("30")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.spawn().unwrap()
    }

    fn spawn_shell(source: String) -> std::process::Child {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(source)
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.spawn().unwrap()
    }

    fn await_file(path: &Path) {
        for _ in 0..200 {
            if path.is_file() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("timed out waiting for {}", path.display());
    }

    fn capture(child: &std::process::Child, role: &str) -> OwnedProcess {
        let candidate = OwnedCandidate::spawned(role, child.id());
        std::thread::sleep(Duration::from_millis(10));
        candidate.finalize().unwrap()
    }

    #[test]
    fn no_record_never_signals_an_unrelated_process() {
        let root = root("unrelated");
        let mut child = spawn_sleep();
        assert_eq!(
            stop_owned(&root, Duration::from_millis(100)).unwrap(),
            StopOutcome::NothingRecorded
        );
        assert!(process_identity(child.id()).is_ok());
        child.kill().unwrap();
        child.wait().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persisted_identity_stops_only_the_owned_group() {
        let root = root("stop");
        let mut child = spawn_sleep();
        let process = capture(&child, "worker");
        persist_owned(&root, vec![process]).unwrap();
        assert_eq!(
            stop_owned(&root, Duration::from_millis(100)).unwrap(),
            StopOutcome::Stopped(1)
        );
        let _ = child.wait();
        assert!(!record_path(&root).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leader_that_handles_sigterm_exits_during_the_grace_without_sigkill() {
        let root = root("graceful");
        let ready = root.join("ready");
        let marker = root.join("term-received");
        let mut child = spawn_shell(format!(
            r#"trap 'printf term > "{}"; exit 0' TERM; printf ready > "{}"; while :; do sleep 1 & wait $!; done"#,
            marker.display(),
            ready.display()
        ));
        await_file(&ready);
        let process = capture(&child, "worker");
        persist_owned(&root, vec![process]).unwrap();
        assert_eq!(
            stop_owned(&root, Duration::from_millis(500)).unwrap(),
            StopOutcome::Stopped(1)
        );
        let status = child.wait().unwrap();
        assert!(
            status.success(),
            "graceful handler did not exit cleanly: {status}"
        );
        assert_eq!(fs::read_to_string(marker).unwrap(), "term");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn leader_ignoring_sigterm_is_killed_after_a_bounded_grace() {
        use std::os::unix::process::ExitStatusExt;
        let root = root("escalate");
        let ready = root.join("ready");
        let mut child = spawn_shell(format!(
            r#"trap '' TERM; printf ready > "{}"; while :; do sleep 1; done"#,
            ready.display()
        ));
        await_file(&ready);
        let process = capture(&child, "engine");
        persist_owned(&root, vec![process]).unwrap();
        let started = Instant::now();
        assert_eq!(
            stop_owned(&root, Duration::from_millis(80)).unwrap(),
            StopOutcome::Stopped(1)
        );
        let elapsed = started.elapsed();
        let status = child.wait().unwrap();
        assert_eq!(
            status.signal(),
            Some(9),
            "leader was not escalated: {status}"
        );
        assert!(
            elapsed >= Duration::from_millis(60),
            "no grace was observed: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "grace was not bounded: {elapsed:?}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn surviving_descendant_after_leader_exit_is_reported_without_blind_signal() {
        let root = root("stubborn-descendant");
        let ready = root.join("ready");
        let descendant_path = root.join("descendant.pid");
        let mut child = spawn_shell(format!(
            r#"trap 'exit 0' TERM; /bin/sh -c 'trap "" TERM; echo $$ > "{}"; while :; do sleep 1; done' & printf ready > "{}"; wait"#,
            descendant_path.display(),
            ready.display()
        ));
        await_file(&ready);
        await_file(&descendant_path);
        let descendant: u32 = fs::read_to_string(&descendant_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let process = capture(&child, "worker");
        let process_group = process.process_group;
        persist_owned(&root, vec![process]).unwrap();
        let error = stop_owned(&root, Duration::from_millis(200))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("descendant cleanup after leader exit is not guaranteed"),
            "{error}"
        );
        assert!(error.contains(&descendant.to_string()), "{error}");
        assert!(
            record_path(&root).is_file(),
            "refusal removed the authority record"
        );
        assert!(process_identity(descendant).is_ok_and(|identity| !identity.zombie));
        unsafe { kill(-(process_group as i32), 9) };
        let _ = child.wait();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stop_signals_descendants_in_the_owned_process_group() {
        let root = root("descendants");
        let descendant_path = root.join("descendant.pid");
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!(
                "sleep 30 & echo $! > '{}'; wait",
                descendant_path.display()
            ))
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        for _ in 0..100 {
            if descendant_path.is_file() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let descendant: u32 = fs::read_to_string(&descendant_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let process = capture(&child, "worker");
        persist_owned(&root, vec![process]).unwrap();
        assert_eq!(
            stop_owned(&root, Duration::from_millis(100)).unwrap(),
            StopOutcome::Stopped(1)
        );
        let _ = child.wait();
        for _ in 0..100 {
            if process_identity(descendant).is_err()
                || process_identity(descendant).is_ok_and(|identity| identity.zombie)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            process_identity(descendant).is_err()
                || process_identity(descendant).is_ok_and(|identity| identity.zombie)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tampered_start_token_refuses_without_signalling() {
        let root = root("tamper");
        let mut child = spawn_sleep();
        let mut process = capture(&child, "engine");
        process.start_token += 1;
        persist_owned(&root, vec![process.clone()]).unwrap();
        // persist filters stale records only when merging; the new record is kept
        // so stop can prove it refuses the mismatch.
        let error = stop_owned(&root, Duration::from_millis(100))
            .unwrap_err()
            .to_string();
        assert!(error.contains("identity no longer matches"), "{error}");
        assert!(
            process_identity(child.id()).is_ok(),
            "unowned process was signalled"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tampered_executable_and_insecure_record_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let root = root("metadata");
        let mut child = spawn_sleep();
        let mut process = capture(&child, "engine");
        process.executable = PathBuf::from("/bin/false");
        persist_owned(&root, vec![process]).unwrap();
        assert!(
            stop_owned(&root, Duration::from_millis(100))
                .unwrap_err()
                .to_string()
                .contains("identity no longer matches")
        );
        fs::set_permissions(record_path(&root), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            stop_owned(&root, Duration::from_millis(100))
                .unwrap_err()
                .to_string()
                .contains("mode 0600")
        );
        child.kill().unwrap();
        child.wait().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
