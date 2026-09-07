use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const RECORD_VERSION: u32 = 1;
const RECORD_RELATIVE_PATH: &str = "run/owned-processes.json";
const LOCK_RELATIVE_PATH: &str = "run/lifecycle.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutableIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug)]
pub(crate) struct LifecycleLock {
    _file: File,
}

impl Drop for LifecycleLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

#[cfg(target_os = "linux")]
fn validate_private_file(path: &Path, metadata: &fs::Metadata, description: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    if !metadata.file_type().is_file()
        || metadata.uid() != effective_uid()?
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        anyhow::bail!(
            "{description} {} must be a single regular file owned by the current user with mode 0600",
            path.display()
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn private_run_dir(agentos_home: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};

    fs::create_dir_all(agentos_home)
        .with_context(|| format!("Cannot create AgentOS home {}", agentos_home.display()))?;
    let run = agentos_home.join("run");
    match fs::symlink_metadata(&run) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::DirBuilder::new().mode(0o700).create(&run) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Cannot create private runtime directory {}", run.display())
                    });
                }
            }
        }
        Err(error) => {
            return Err(error).with_context(|| format!("Cannot inspect {}", run.display()));
        }
    }
    let metadata = fs::symlink_metadata(&run)?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid()?
        || metadata.mode() & 0o777 != 0o700
    {
        anyhow::bail!(
            "AgentOS runtime directory {} must be a private directory owned by the current user with mode 0700",
            run.display()
        );
    }
    Ok(run)
}

#[cfg(target_os = "linux")]
pub(crate) fn try_lock(agentos_home: &Path) -> Result<LifecycleLock> {
    use std::os::unix::fs::OpenOptionsExt;

    let _run = private_run_dir(agentos_home)?;
    let path = agentos_home.join(LOCK_RELATIVE_PATH);
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        validate_private_file(&path, &metadata, "Lifecycle lock")?;
    }
    // Linux O_NOFOLLOW | O_CLOEXEC. The lock inode is permanent; dropping the
    // descriptor releases the advisory lock without a stale sentinel race.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(0o400000 | 0o2000000)
        .open(&path)
        .with_context(|| format!("Cannot open lifecycle lock {}", path.display()))?;
    let file_metadata = file.metadata()?;
    validate_private_file(&path, &file_metadata, "Lifecycle lock")?;
    {
        use std::os::unix::fs::MetadataExt;
        let path_metadata = fs::symlink_metadata(&path)?;
        if path_metadata.dev() != file_metadata.dev() || path_metadata.ino() != file_metadata.ino()
        {
            anyhow::bail!("Lifecycle lock path changed while it was opened");
        }
    }
    file.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => anyhow::anyhow!(
            "AgentOS lifecycle is busy for {}; another start, up, or stop transaction is active",
            agentos_home.display()
        ),
        fs::TryLockError::Error(error) => {
            anyhow::Error::new(error).context("Cannot acquire AgentOS lifecycle lock")
        }
    })?;
    Ok(LifecycleLock { _file: file })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn try_lock(_agentos_home: &Path) -> Result<LifecycleLock> {
    ensure_supported()?;
    unreachable!()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForegroundLockPolicy {
    Exclusive,
    NoPlatformLock,
}

const fn foreground_lock_policy(is_linux: bool) -> ForegroundLockPolicy {
    if is_linux {
        ForegroundLockPolicy::Exclusive
    } else {
        ForegroundLockPolicy::NoPlatformLock
    }
}

/// Serialize foreground startup on Linux without routing supported non-Linux
/// foreground mode through the detached-lifecycle refusal.
pub(crate) fn try_lock_foreground(agentos_home: &Path) -> Result<Option<LifecycleLock>> {
    match foreground_lock_policy(cfg!(target_os = "linux")) {
        ForegroundLockPolicy::Exclusive => {
            #[cfg(target_os = "linux")]
            return try_lock(agentos_home).map(Some);
            #[cfg(not(target_os = "linux"))]
            unreachable!("non-Linux cannot select the exclusive Linux lock policy");
        }
        ForegroundLockPolicy::NoPlatformLock => {
            let _ = agentos_home;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod platform_policy_tests {
    use super::*;

    #[test]
    fn foreground_start_never_routes_non_linux_through_detached_refusal() {
        assert_eq!(
            foreground_lock_policy(false),
            ForegroundLockPolicy::NoPlatformLock
        );
        assert_eq!(
            foreground_lock_policy(true),
            ForegroundLockPolicy::Exclusive
        );
    }
}

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
            executable_identity: observed.executable_identity,
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
    /// Kernel path retained for diagnostics and legacy records. New records pin
    /// the executable inode so moving or replacing a binary preserves ownership.
    pub(crate) executable: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    executable_identity: Option<ExecutableIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnershipRecord {
    version: u32,
    processes: Vec<OwnedProcess>,
}

#[derive(Debug)]
struct RecordGeneration {
    device: u64,
    inode: u64,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct LoadedRecord {
    record: OwnershipRecord,
    generation: RecordGeneration,
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
    executable_identity: Option<ExecutableIdentity>,
    executable_deleted: bool,
    zombie: bool,
}

#[cfg(target_os = "linux")]
fn process_state_is_dead(state: &str) -> bool {
    matches!(state, "Z" | "X" | "x")
}

#[cfg(target_os = "linux")]
pub(crate) fn procfs_entry_gone(error: &std::io::Error) -> bool {
    // procfs may return ESRCH after lookup when a task exits before the read.
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(3)
}

#[cfg(target_os = "linux")]
fn parse_process_stat(stat: &str, stat_path: &Path) -> Result<(u64, bool)> {
    let (_, fields) = stat
        .rsplit_once(") ")
        .with_context(|| format!("Malformed process identity {}", stat_path.display()))?;
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
    Ok((start_token, process_state_is_dead(state)))
}

#[cfg(target_os = "linux")]
fn process_identity_at(
    pid: u32,
    stat_path: &Path,
    executable_path: &Path,
) -> Result<Option<ProcessIdentity>> {
    use std::os::unix::fs::MetadataExt;
    let stat = match fs::read_to_string(stat_path) {
        Ok(stat) => stat,
        Err(error) if procfs_entry_gone(&error) => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Cannot read process identity {}", stat_path.display()));
        }
    };
    let (start_token, zombie) = parse_process_stat(&stat, stat_path)?;
    if zombie {
        return Ok(Some(ProcessIdentity {
            start_token,
            executable: PathBuf::new(),
            executable_identity: None,
            executable_deleted: false,
            zombie: true,
        }));
    }
    let executable = match fs::read_link(executable_path) {
        Ok(executable) => executable,
        Err(error) if procfs_entry_gone(&error) => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Cannot resolve executable for pid {pid}"));
        }
    };
    let metadata = match fs::metadata(executable_path) {
        Ok(metadata) => metadata,
        // Exit can remove the executable between read_link and stat. An absent
        // inode is not a live, mismatching identity. Unlinked live images still
        // have kernel metadata through the /proc executable magic link.
        Err(error) if procfs_entry_gone(&error) => return Ok(None),
        Err(error) => return Err(error).context("Cannot inspect executable identity"),
    };
    Ok(Some(ProcessIdentity {
        start_token,
        executable,
        executable_identity: Some(ExecutableIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        }),
        executable_deleted: metadata.nlink() == 0,
        zombie: false,
    }))
}

#[cfg(target_os = "linux")]
fn process_identity_if_present(pid: u32) -> Result<Option<ProcessIdentity>> {
    let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
    let executable_path = PathBuf::from(format!("/proc/{pid}/exe"));
    process_identity_at(pid, &stat_path, &executable_path)
}

#[cfg(target_os = "linux")]
fn process_identity(pid: u32) -> Result<ProcessIdentity> {
    process_identity_if_present(pid)?
        .with_context(|| format!("Process {pid} exited before its identity could be read"))
}

#[cfg(target_os = "linux")]
fn recorded_identity_matches(process: &OwnedProcess, identity: &ProcessIdentity) -> bool {
    use std::os::unix::ffi::OsStrExt;

    if identity.start_token != process.start_token {
        return false;
    }
    if let Some(expected) = process.executable_identity {
        return identity.executable_identity == Some(expected);
    }
    // Version-1 records without an inode remain readable. Only the kernel's
    // unlinked-file suffix is an alias, not a real filename ending in that text.
    identity.executable == process.executable
        || (identity.executable_deleted
            && identity
                .executable
                .as_os_str()
                .as_bytes()
                .strip_suffix(b" (deleted)")
                == Some(process.executable.as_os_str().as_bytes()))
}

#[cfg(target_os = "linux")]
fn group_has_owned_member(process: &OwnedProcess, witnesses: &[OwnedProcess]) -> Result<bool> {
    if verify_identity(process)? {
        return Ok(true);
    }
    for witness in witnesses
        .iter()
        .filter(|member| member.process_group == process.process_group)
    {
        let path = PathBuf::from(format!("/proc/{}/stat", witness.pid));
        let stat = match fs::read_to_string(&path) {
            Ok(stat) => stat,
            Err(error) if procfs_entry_gone(&error) => continue,
            Err(error) => return Err(error).context("Cannot inspect owned group witness"),
        };
        let (start_token, dead) = parse_process_stat(&stat, &path)?;
        let group = stat
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .nth(2)
            .context("Missing witness process group")?
            .parse::<u32>()?;
        // These witnesses were captured through verified ancestry in THIS stop.
        // PID birth and current group membership preserve ownership across exec.
        if !dead && start_token == witness.start_token && group == process.process_group {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn supervisor_cleanup_grace(processes: &[OwnedProcess], requested: Duration) -> Result<Duration> {
    for process in processes.iter().filter(|process| process.role == "engine") {
        if !verify_identity(process)? {
            continue;
        }
        let command = match fs::read(format!("/proc/{}/cmdline", process.pid)) {
            Ok(command) => command,
            Err(error) if procfs_entry_gone(&error) => continue,
            Err(error) => return Err(error).context("Cannot inspect engine cleanup mode"),
        };
        if command.split(|byte| *byte == 0).nth(1) == Some(crate::supervisor::MODE.as_bytes()) {
            // The supervisor needs its bounded engine/descendant reap budget even
            // when the operator requests immediate escalation for ordinary workers.
            return Ok(requested.max(Duration::from_secs(5)));
        }
    }
    Ok(requested)
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

fn read_record(agentos_home: &Path) -> Result<Option<LoadedRecord>> {
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
    #[cfg(target_os = "linux")]
    let metadata_before = fs::symlink_metadata(&path)?;
    let bytes = fs::read(&path).with_context(|| format!("Cannot read {}", path.display()))?;
    #[cfg(target_os = "linux")]
    let metadata_after = fs::symlink_metadata(&path)?;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata_before.dev() != metadata_after.dev()
            || metadata_before.ino() != metadata_after.ino()
        {
            anyhow::bail!("Lifecycle record changed while it was being read");
        }
    }
    let record: OwnershipRecord = serde_json::from_slice(&bytes)
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
    #[cfg(target_os = "linux")]
    let generation = {
        use std::os::unix::fs::MetadataExt;
        RecordGeneration {
            device: metadata_after.dev(),
            inode: metadata_after.ino(),
            bytes,
        }
    };
    #[cfg(not(target_os = "linux"))]
    let generation = RecordGeneration {
        device: 0,
        inode: 0,
        bytes,
    };
    Ok(Some(LoadedRecord { record, generation }))
}

/// Capture detached groups only through the ancestry of a verified owned process.
#[cfg(target_os = "linux")]
fn capture_descendant_groups(owned: &mut Vec<OwnedProcess>) -> Result<Vec<OwnedProcess>> {
    let mut pending = owned.clone();
    let mut witnesses = Vec::new();
    let mut seen = owned
        .iter()
        .map(|process| process.pid)
        .collect::<BTreeSet<_>>();
    for process in &pending {
        verify_identity(process)?;
    }
    while let Some(parent) = pending.pop() {
        if !verify_identity(&parent)? {
            continue;
        }
        // /proc task children proves ancestry even when a child called setsid.
        // A pathname, executable match, or stale pid alone never grants ownership.
        let tasks = match fs::read_dir(format!("/proc/{}/task", parent.pid)) {
            Ok(tasks) => tasks,
            Err(error) if procfs_entry_gone(&error) => continue,
            Err(error) => return Err(error).context("Cannot inspect owned process tasks"),
        };
        for task in tasks {
            let task = match task {
                Ok(task) => task,
                Err(error) if procfs_entry_gone(&error) => continue,
                Err(error) => return Err(error).context("Cannot inspect owned task entry"),
            };
            let path = task.path().join("children");
            let children = match fs::read_to_string(&path) {
                Ok(children) => children,
                Err(error) if procfs_entry_gone(&error) => continue,
                Err(error) => return Err(error).context("Cannot inspect owned process children"),
            };
            for child in children.split_whitespace() {
                let pid = child.parse::<u32>().context("Invalid child pid")?;
                if seen.contains(&pid) {
                    continue;
                }
                let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
                let stat = match fs::read_to_string(&stat_path) {
                    Ok(stat) => stat,
                    Err(error) if procfs_entry_gone(&error) => continue,
                    Err(error) => return Err(error).context("Cannot inspect child identity"),
                };
                let (start_token, dead) = parse_process_stat(&stat, &stat_path)?;
                let fields = stat
                    .rsplit_once(") ")
                    .unwrap()
                    .1
                    .split_whitespace()
                    .collect::<Vec<_>>();
                let parent_pid = fields
                    .get(1)
                    .context("Missing parent pid")?
                    .parse::<u32>()?;
                let process_group = fields
                    .get(2)
                    .context("Missing process group")?
                    .parse::<u32>()?;
                if dead || parent_pid != parent.pid || start_token < parent.start_token {
                    continue;
                }
                let Some(identity) = process_identity_if_present(pid)? else {
                    continue;
                };
                if identity.zombie
                    || identity.start_token != start_token
                    || !verify_identity(&parent)?
                {
                    continue;
                }
                if !seen.insert(pid) {
                    continue;
                }
                if seen.len() > 1024 {
                    anyhow::bail!("Owned descendant scan exceeds its process limit");
                }
                let descendant = OwnedProcess {
                    role: "worker".to_owned(),
                    pid,
                    process_group,
                    start_token,
                    executable: identity.executable,
                    executable_identity: identity.executable_identity,
                };
                // Inherited groups are already covered by their original leader;
                // continue through them to discover nested detached groups.
                if process_group == pid {
                    if owned.len() >= 128 {
                        anyhow::bail!("Lifecycle record contains too many processes");
                    }
                    owned.push(descendant.clone());
                }
                witnesses.push(descendant.clone());
                pending.push(descendant);
            }
        }
    }
    Ok(witnesses)
}

/// Keep daemonized grandchildren attached to their supervisor for its entire
/// lifetime. This must run before the engine can fork helpers.
#[cfg(target_os = "linux")]
pub(crate) fn enable_child_subreaper() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            fn prctl(option: i32, ...) -> i32;
        }
        const PR_SET_CHILD_SUBREAPER: i32 = 36;
        if unsafe { prctl(PR_SET_CHILD_SUBREAPER, 1_usize, 0_usize, 0_usize, 0_usize) } != 0 {
            return Err(std::io::Error::last_os_error())
                .context("Cannot adopt engine registry daemons");
        }
    }
    Ok(())
}

/// Capture orphan identities in the isolated subreaper regression fixture.
#[cfg(all(test, target_os = "linux"))]
fn capture_adopted_descendants(owned: &mut Vec<OwnedProcess>) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let root = OwnedCandidate::spawned("worker", std::process::id()).finalize()?;
        let mut tree = vec![root];
        capture_descendant_groups(&mut tree)?;
        let mut seen = owned
            .iter()
            .map(|process| process.pid)
            .collect::<BTreeSet<_>>();
        for child in tree.into_iter().skip(1) {
            if seen.insert(child.pid) {
                owned.push(child);
            }
        }
        if owned.len() > 128 {
            anyhow::bail!("Lifecycle record contains too many processes");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = owned;
    Ok(())
}

pub(crate) fn terminate_spawned_group(
    child: &mut std::process::Child,
    grace: Duration,
) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        let owned = OwnedCandidate::spawned("engine", child.id()).finalize()?;
        signal_verified_group(&owned, &[], 15)?;
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        signal_verified_group(&owned, &[], 9)?;
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = grace;
        child.kill()?;
    }
    child.wait()?;
    Ok(())
}

/// Persist one startup transaction while the caller holds its `LifecycleLock`.
pub(crate) fn persist_owned(agentos_home: &Path, mut started: Vec<OwnedProcess>) -> Result<()> {
    ensure_supported()?;
    if started.is_empty() {
        return Ok(());
    }
    if let Some(existing) = read_record(agentos_home)? {
        for process in existing.record.processes {
            #[cfg(target_os = "linux")]
            match process_identity_if_present(process.pid)? {
                None => {} // a fully exited prior process is stale, not authority
                Some(identity) if identity.zombie => {}
                Some(identity) if recorded_identity_matches(&process, &identity) => {
                    started.push(process);
                }
                Some(_) => anyhow::bail!(
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
    #[cfg(target_os = "linux")]
    let parent = private_run_dir(agentos_home)?;
    #[cfg(not(target_os = "linux"))]
    let parent = path
        .parent()
        .context("Lifecycle record has no parent")?
        .to_path_buf();
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
fn verify_identity(process: &OwnedProcess) -> Result<bool> {
    let Some(identity) = process_identity_if_present(process.pid)? else {
        return Ok(false);
    };
    if identity.zombie {
        return Ok(false);
    }
    if !recorded_identity_matches(process, &identity) {
        anyhow::bail!(
            "refusing pid {} for role {}: recorded process identity no longer matches",
            process.pid,
            process.role
        );
    }
    Ok(true)
}

#[cfg(target_os = "linux")]
fn leader_is_running(process: &OwnedProcess) -> Result<bool> {
    verify_identity(process)
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn renameat2(
        olddirfd: i32,
        oldpath: *const std::ffi::c_char,
        newdirfd: i32,
        newpath: *const std::ffi::c_char,
        flags: u32,
    ) -> i32;
}

#[cfg(target_os = "linux")]
fn signal_verified_group(
    process: &OwnedProcess,
    witnesses: &[OwnedProcess],
    signal: i32,
) -> Result<()> {
    if !group_has_owned_member(process, witnesses)? {
        return Ok(());
    }
    let group = i32::try_from(process.process_group).context("process group does not fit i32")?;
    // Linux/POSIX: a negative pid addresses exactly that process group.
    if unsafe { kill(-group, signal) } != 0 && group_has_owned_member(process, witnesses)? {
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
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if procfs_entry_gone(&error) => continue,
            Err(error) => {
                return Err(error).context("Cannot inspect /proc entry for owned descendants");
            }
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let stat_path = entry.path().join("stat");
        let stat_bytes = match fs::read(&stat_path) {
            Ok(stat) => stat,
            Err(error) if procfs_entry_gone(&error) => continue,
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
        if group == process_group && !process_state_is_dead(state) {
            members.push(pid);
        }
    }
    members.sort_unstable();
    Ok(members)
}

#[cfg(target_os = "linux")]
fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let from = std::ffi::CString::new(from.as_os_str().as_bytes())
        .context("Lifecycle record source path contains NUL")?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes())
        .context("Lifecycle record capture path contains NUL")?;
    // AT_FDCWD, RENAME_NOREPLACE: capture the exact pathname generation without
    // overwriting any competing writer's file.
    if unsafe { renameat2(-100, from.as_ptr(), -100, to.as_ptr(), 1) } != 0 {
        return Err(std::io::Error::last_os_error()).context("Cannot capture lifecycle record");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_record_if_generation_matches(
    agentos_home: &Path,
    generation: &RecordGeneration,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let path = record_path(agentos_home);
    let capture = path.with_file_name(format!(
        ".owned-processes.stop.{}.{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    rename_noreplace(&path, &capture)?;
    let validation = (|| -> Result<()> {
        validate_record_file(&capture)?;
        let metadata = fs::symlink_metadata(&capture)?;
        let bytes = fs::read(&capture)?;
        if metadata.dev() != generation.device
            || metadata.ino() != generation.inode
            || bytes != generation.bytes
        {
            anyhow::bail!("captured lifecycle record is not the generation stop validated");
        }
        Ok(())
    })();
    if let Err(error) = validation {
        if let Err(restore_error) = rename_noreplace(&capture, &path) {
            anyhow::bail!(
                "lifecycle record changed during stop; newer generation preserved at {} and captured record preserved at {} ({error:#}; restore failed: {restore_error:#})",
                path.display(),
                capture.display()
            );
        }
        anyhow::bail!(
            "lifecycle record changed during stop; newer generation restored at {} ({error:#})",
            path.display()
        );
    }
    fs::remove_file(&capture).context("Cannot remove captured lifecycle record after stop")
}

pub(crate) fn stop_owned(agentos_home: &Path, grace: Duration) -> Result<StopOutcome> {
    ensure_supported()?;
    let _lifecycle_lock = try_lock(agentos_home)?;
    let Some(loaded) = read_record(agentos_home)? else {
        return Ok(StopOutcome::NothingRecorded);
    };
    let mut record = loaded.record;
    let generation = loaded.generation;
    #[cfg(target_os = "linux")]
    let witnesses = capture_descendant_groups(&mut record.processes)?;
    // Validate every recorded root before signalling anything. Witnesses only
    // come from the verified ancestry walk above, never from stale group ids.
    #[cfg(target_os = "linux")]
    for process in &record.processes {
        let _ = verify_identity(process)?;
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
        let grace = supervisor_cleanup_grace(&record.processes, grace)?;
        for process in &record.processes {
            signal_verified_group(process, &witnesses, 15)?;
        }
        let deadline = Instant::now() + grace;
        loop {
            let mut running = false;
            for process in &record.processes {
                running |= group_has_owned_member(process, &witnesses)?;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !running || remaining.is_zero() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20).min(remaining));
        }
        // A live, ancestry-captured member pins ownership even if the original
        // leader handled SIGTERM and exited. Never signal an unwitnessed group.
        for process in &record.processes {
            signal_verified_group(process, &witnesses, 9)?;
        }
        let kill_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let mut running = false;
            for process in &record.processes {
                running |= group_has_owned_member(process, &witnesses)?;
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
    #[cfg(target_os = "linux")]
    remove_record_if_generation_matches(agentos_home, &generation)?;
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

    fn spawn_copied_binary(path: &Path) -> std::process::Child {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match Command::new(path).arg("30").process_group(0).spawn() {
                Ok(child) => return child,
                // Parallel tests can briefly inherit the copy's write descriptor
                // between fork and exec, even after this thread has closed it.
                Err(error) if error.raw_os_error() == Some(26) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("cannot spawn copied fixture {}: {error}", path.display()),
            }
        }
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

    fn spawn_stubborn_descendant(pid_file: &Path) -> std::process::Child {
        // Readiness precedes a blocking builtin, with no further fork/exec races.
        Command::new("/bin/sh")
        .args(["-c", &format!(r#"exec 3<&0; trap 'exit 0' TERM; /bin/sh -c 'trap "" TERM; echo $$ > "{}"; read blocked <&3' & wait"#, pid_file.display())])
        .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null())
        .process_group(0).spawn().unwrap()
    }

    fn await_file(path: &Path) {
        for _ in 0..200 {
            if fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0) {
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

    fn detached_registry_fixture(root: &Path) -> (std::process::Child, OwnedProcess) {
        let ready = root.join("registry.pid");
        let engine = spawn_shell(format!(
            "setsid /bin/sh -c 'echo $$ > {}; trap \"exit 0\" TERM; while :; do sleep 0.1; done' & wait",
            ready.display()
        ));
        await_file(&ready);
        let pid = fs::read_to_string(&ready).unwrap().trim().parse().unwrap();
        let registry = OwnedCandidate::spawned("worker", pid).finalize().unwrap();
        (engine, registry)
    }

    #[test]
    fn reparented_registry_is_adopted_and_persisted() {
        const CHILD: &str = "AGENTOS_SUBREAPER_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "lifecycle::tests::reparented_registry_is_adopted_and_persisted",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success(), "isolated subreaper fixture failed");
            return;
        }
        enable_child_subreaper().unwrap();
        let root = root("reparented-registry");
        let ready = root.join("registry.pid");
        let mut engine = spawn_shell(format!(
            "(setsid /bin/sh -c 'echo $$ > {}; trap \"exit 0\" TERM; while :; do sleep 0.1; done' &) ; exec sleep 30",
            ready.display()
        ));
        await_file(&ready);
        let pid = fs::read_to_string(&ready).unwrap().trim().parse().unwrap();
        let registry = OwnedCandidate::spawned("worker", pid).finalize().unwrap();
        let mut owned = vec![capture(&engine, "engine")];
        capture_adopted_descendants(&mut owned).unwrap();
        let recorded = owned.iter().any(|process| process.pid == pid);
        persist_owned(&root, owned).unwrap();
        engine.kill().unwrap();
        engine.wait().unwrap();
        let result = stop_owned(&root, Duration::from_secs(1));
        let survived = verify_identity(&registry).unwrap();
        signal_verified_group(&registry, &[], 9).unwrap();
        fs::remove_dir_all(root).unwrap();
        assert!(recorded, "daemon reparented during boot was not captured");
        assert!(result.is_ok(), "{result:?}");
        assert!(!survived, "adopted registry survived stop");
    }

    #[test]
    fn detached_registry_is_recorded_and_stopped_after_engine_exit() {
        let root = root("detached-registry");
        let (mut engine, registry) = detached_registry_fixture(&root);
        let mut unrelated = spawn_sleep();
        let mut owned = vec![capture(&engine, "engine")];
        capture_descendant_groups(&mut owned).unwrap();
        persist_owned(&root, owned).unwrap();
        let recorded = read_record(&root)
            .unwrap()
            .unwrap()
            .record
            .processes
            .iter()
            .any(|process| process.pid == registry.pid);
        engine.kill().unwrap();
        engine.wait().unwrap();
        let result = stop_owned(&root, Duration::from_secs(1));
        let survived = verify_identity(&registry).unwrap();
        let untouched = unrelated.try_wait().unwrap().is_none();
        signal_verified_group(&registry, &[], 9).unwrap();
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        fs::remove_dir_all(&root).unwrap();
        assert!(recorded, "detached registry ownership was not persisted");
        assert!(result.is_ok(), "{result:?}");
        assert!(!survived, "detached registry survived CLI stop");
        assert!(untouched, "unrelated process was stopped");
    }

    #[test]
    fn detached_registry_started_after_persistence_is_discovered_before_stop() {
        let root = root("late-detached-registry");
        let ready = root.join("registry.pid");
        let launch = root.join("launch");
        let mut engine = spawn_shell(format!(
            "while [ ! -f {} ]; do sleep 0.01; done; setsid /bin/sh -c 'echo $$ > {}; trap \"exit 0\" TERM; while :; do sleep 0.1; done' & wait",
            launch.display(),
            ready.display()
        ));
        persist_owned(&root, vec![capture(&engine, "engine")]).unwrap();
        fs::write(&launch, "go").unwrap();
        await_file(&ready);
        let pid = fs::read_to_string(&ready).unwrap().trim().parse().unwrap();
        let registry = OwnedCandidate::spawned("worker", pid).finalize().unwrap();
        let result = stop_owned(&root, Duration::from_secs(1));
        let survived = verify_identity(&registry).unwrap();
        signal_verified_group(&registry, &[], 9).unwrap();
        let _ = engine.kill();
        engine.wait().unwrap();
        fs::remove_dir_all(&root).unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert!(!survived, "late detached registry survived CLI stop");
    }

    #[test]
    fn lifecycle_lock_is_exclusive_and_releases_without_unlinking_its_inode() {
        let root = root("lock-exclusive");
        let holder = try_lock(&root).unwrap();
        let error = try_lock(&root).unwrap_err().to_string();
        assert!(error.contains("lifecycle is busy"), "{error}");
        let lock_path = root.join(LOCK_RELATIVE_PATH);
        let inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&lock_path).unwrap().ino()
        };
        drop(holder);
        let released = try_lock(&root).unwrap();
        assert_eq!(
            {
                use std::os::unix::fs::MetadataExt;
                fs::metadata(&lock_path).unwrap().ino()
            },
            inode,
            "lock release unlinked the permanent lock inode"
        );
        drop(released);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn group_scan_excludes_every_linux_dead_process_state() {
        for state in ["Z", "X", "x"] {
            assert!(
                process_state_is_dead(state),
                "group scan treated {state} as a live descendant"
            );
        }
        for state in ["R", "S", "D", "T", "t", "W", "I"] {
            assert!(
                !process_state_is_dead(state),
                "live state {state} was excluded"
            );
        }
    }

    #[test]
    fn identity_reader_treats_only_confirmed_disappearance_as_gone() {
        for errno in [2, 3] {
            assert!(procfs_entry_gone(&std::io::Error::from_raw_os_error(errno)));
        }
        for errno in [1, 5, 13, 20, 22] {
            assert!(!procfs_entry_gone(&std::io::Error::from_raw_os_error(
                errno
            )));
        }
        let root = root("identity-read-races");
        let executable = root.join("exe");
        fs::write(&executable, "fixture").unwrap();
        assert!(
            process_identity_at(42, &root.join("missing-stat"), &executable)
                .unwrap()
                .is_none(),
            "a missing stat file is a process that has already gone"
        );

        let stat_dir = root.join("stat-directory");
        fs::create_dir(&stat_dir).unwrap();
        let error = process_identity_at(42, &stat_dir, &executable)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Cannot read process identity"), "{error}");

        let stat_path = root.join("stat");
        fs::copy("/proc/self/stat", &stat_path).unwrap();
        assert!(
            process_identity_at(42, &stat_path, &root.join("missing-exe"))
                .unwrap()
                .is_none(),
            "a vanished /proc executable link is a process that has gone"
        );
        let executable_dir = root.join("exe-directory");
        fs::create_dir(&executable_dir).unwrap();
        let error = process_identity_at(42, &stat_path, &executable_dir)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Cannot resolve executable"), "{error}");
        let deleted_target = root.join("deleted-target");
        let deleted_link = root.join("deleted-executable-link");
        std::os::unix::fs::symlink(&deleted_target, &deleted_link).unwrap();
        assert!(
            process_identity_at(42, &stat_path, &deleted_link)
                .unwrap()
                .is_none(),
            "a link without executable metadata must not fabricate a live identity"
        );

        fs::remove_file(&stat_path).unwrap();
        fs::write(&stat_path, "malformed").unwrap();
        let error = process_identity_at(42, &stat_path, &executable)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Malformed process identity"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn process_that_vanished_before_stop_is_retired_as_gone() {
        let root = root("vanished-before-stop");
        let mut child = spawn_sleep();
        let process = capture(&child, "worker");
        persist_owned(&root, vec![process]).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();

        let outcome = stop_owned(&root, Duration::from_millis(100));
        let record_remains = record_path(&root).exists();
        fs::remove_dir_all(root).unwrap();

        assert_eq!(outcome.unwrap(), StopOutcome::Stopped(1));
        assert!(!record_remains, "vanished ownership record was preserved");
    }

    #[test]
    fn busy_stop_refuses_before_signalling_or_changing_the_record() {
        let root = root("lock-stop");
        let mut child = spawn_sleep();
        let process = capture(&child, "worker");
        persist_owned(&root, vec![process]).unwrap();
        let record_before = fs::read(record_path(&root)).unwrap();
        let holder = try_lock(&root).unwrap();
        let started = Instant::now();
        let error = stop_owned(&root, Duration::from_millis(100))
            .unwrap_err()
            .to_string();
        assert!(error.contains("lifecycle is busy"), "{error}");
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(
            process_identity(child.id()).is_ok(),
            "busy stop signalled the leader"
        );
        assert_eq!(fs::read(record_path(&root)).unwrap(), record_before);
        drop(holder);
        assert_eq!(
            stop_owned(&root, Duration::from_millis(100)).unwrap(),
            StopOutcome::Stopped(1)
        );
        let _ = child.wait();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_record_generation_is_restored_instead_of_deleted() {
        let root = root("record-generation");
        let mut first = spawn_sleep();
        persist_owned(&root, vec![capture(&first, "worker")]).unwrap();
        let loaded = read_record(&root).unwrap().unwrap();
        let mut second = spawn_sleep();
        persist_owned(&root, vec![capture(&second, "engine")]).unwrap();

        let error = remove_record_if_generation_matches(&root, &loaded.generation)
            .unwrap_err()
            .to_string();
        assert!(error.contains("changed during stop"), "{error}");
        let current = read_record(&root).unwrap().unwrap();
        assert_eq!(current.record.processes.len(), 2);
        assert!(record_path(&root).is_file());

        first.kill().unwrap();
        second.kill().unwrap();
        first.wait().unwrap();
        second.wait().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lock_refuses_insecure_run_directory_and_symlink_path() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let insecure = root("lock-insecure-dir");
        let run = insecure.join("run");
        fs::create_dir(&run).unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            try_lock(&insecure)
                .unwrap_err()
                .to_string()
                .contains("mode 0700")
        );
        fs::remove_dir_all(insecure).unwrap();

        let linked = root("lock-symlink");
        let run = linked.join("run");
        fs::create_dir(&run).unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o700)).unwrap();
        symlink("/tmp", run.join("lifecycle.lock")).unwrap();
        assert!(
            try_lock(&linked)
                .unwrap_err()
                .to_string()
                .contains("regular file")
        );
        fs::remove_dir_all(linked).unwrap();
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
    fn captured_descendant_is_stopped_after_its_leader_exits() {
        let root = root("stubborn-descendant");
        let descendant_path = root.join("descendant.pid");
        let mut child = spawn_stubborn_descendant(&descendant_path);
        await_file(&descendant_path);
        let descendant: u32 = fs::read_to_string(&descendant_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let process = capture(&child, "worker");
        let group = process.process_group;
        persist_owned(&root, vec![process]).unwrap();
        let outcome = stop_owned(&root, Duration::from_millis(100));
        let still_running = process_identity(descendant).is_ok_and(|identity| !identity.zombie);
        let record_remains = record_path(&root).exists();
        unsafe { kill(-(group as i32), 9) };
        let _ = child.wait();
        fs::remove_dir_all(root).unwrap();
        assert_eq!(outcome.unwrap(), StopOutcome::Stopped(1));
        assert!(!still_running, "verified descendant survived escalation");
        assert!(
            !record_remains,
            "successful cleanup left an un-retryable record"
        );
    }

    fn replaced_binary_fixture(legacy: bool, rename_only: bool) {
        let root = root("replaced-executable");
        let binary = root.join("owned-binary");
        fs::copy("/bin/sleep", &binary).unwrap();
        let mut child = spawn_copied_binary(&binary);
        let mut process = capture(&child, "engine");
        if legacy {
            process.executable_identity = None;
        }
        persist_owned(&root, vec![process.clone()]).unwrap();
        let replacement = root.join("replacement");
        if rename_only {
            fs::rename(&binary, &replacement).unwrap();
        } else {
            fs::copy("/bin/sleep", &replacement).unwrap();
            fs::rename(&replacement, &binary).unwrap();
        }
        let matches = verify_identity(&process);
        let persisted = persist_owned(&root, vec![process]);
        let outcome = stop_owned(&root, Duration::from_millis(100));
        let _ = child.kill();
        let _ = child.wait();
        fs::remove_dir_all(root).unwrap();
        assert!(
            matches.unwrap(),
            "replacing or moving an executable lost ownership"
        );
        persisted.unwrap();
        assert_eq!(outcome.unwrap(), StopOutcome::Stopped(1));
    }

    #[test]
    fn replaced_running_executable_remains_stoppable_by_inode() {
        replaced_binary_fixture(false, false);
        replaced_binary_fixture(false, true);
    }

    #[test]
    fn legacy_record_accepts_the_kernel_unlinked_suffix() {
        replaced_binary_fixture(true, false);
    }

    #[test]
    fn a_real_deleted_suffix_is_not_a_legacy_executable_alias() {
        let root = root("literal-deleted-suffix");
        let binary = root.join("owned-binary (deleted)");
        fs::copy("/bin/sleep", &binary).unwrap();
        let mut child = spawn_copied_binary(&binary);
        let mut process = capture(&child, "worker");
        process.executable_identity = None;
        process.executable = root.join("owned-binary");
        persist_owned(&root, vec![process]).unwrap();
        let outcome = stop_owned(&root, Duration::from_millis(20));
        let survived = child.try_wait().unwrap().is_none();
        let record_remains = record_path(&root).exists();
        let _ = child.kill();
        let _ = child.wait();
        fs::remove_dir_all(root).unwrap();
        assert!(outcome.is_err());
        assert!(survived && record_remains);
    }

    #[test]
    fn a_group_without_a_live_recorded_leader_or_captured_witness_is_refused() {
        let root = root("unwitnessed-group");
        let ready = root.join("descendant.pid");
        let mut child = spawn_stubborn_descendant(&ready);
        await_file(&ready);
        let descendant: u32 = fs::read_to_string(&ready).unwrap().trim().parse().unwrap();
        let process = capture(&child, "worker");
        let group = process.process_group;
        persist_owned(&root, vec![process]).unwrap();
        child.kill().unwrap();
        for _ in 0..200 {
            if process_identity(child.id()).is_ok_and(|identity| identity.zombie) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let outcome = stop_owned(&root, Duration::from_millis(20));
        let survived = process_identity(descendant).is_ok_and(|identity| !identity.zombie);
        let record_remains = record_path(&root).exists();
        // The unreaped fixture Child still reserves the original group id.
        unsafe { kill(-(group as i32), 9) };
        let _ = child.wait();
        fs::remove_dir_all(root).unwrap();
        assert!(outcome.is_err());
        assert!(survived && record_remains);
    }

    #[test]
    fn supervisor_cleanup_keeps_its_budget_when_a_short_grace_is_requested() {
        let root = root("supervisor-budget");
        fs::write(root.join(crate::supervisor::MODE), "sleep 30\n").unwrap();
        let mut child = Command::new("/bin/sh")
            .arg(crate::supervisor::MODE)
            .current_dir(&root)
            .process_group(0)
            .spawn()
            .unwrap();
        let process = capture(&child, "engine");
        let short = supervisor_cleanup_grace(std::slice::from_ref(&process), Duration::ZERO);
        let long = supervisor_cleanup_grace(std::slice::from_ref(&process), Duration::from_secs(9));
        unsafe { kill(-(process.process_group as i32), 9) };
        let _ = child.wait();
        fs::remove_dir_all(root).unwrap();
        assert_eq!(short.unwrap(), Duration::from_secs(5));
        assert_eq!(long.unwrap(), Duration::from_secs(9));
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
        persist_owned(&root, vec![capture(&child, "engine")]).unwrap();
        let mut record = read_record(&root).unwrap().unwrap().record;
        record.processes[0].start_token += 1;
        fs::write(record_path(&root), serde_json::to_vec(&record).unwrap()).unwrap();
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
        persist_owned(&root, vec![capture(&child, "engine")]).unwrap();
        let mut record = read_record(&root).unwrap().unwrap().record;
        record.processes[0]
            .executable_identity
            .as_mut()
            .unwrap()
            .inode ^= 1;
        fs::write(record_path(&root), serde_json::to_vec(&record).unwrap()).unwrap();
        let bad_inode = stop_owned(&root, Duration::from_millis(100));
        let mut legacy = capture(&child, "engine");
        legacy.executable_identity = None;
        legacy.executable = PathBuf::from("/bin/false");
        record.processes[0] = legacy;
        fs::write(record_path(&root), serde_json::to_vec(&record).unwrap()).unwrap();
        let bad_legacy_path = stop_owned(&root, Duration::from_millis(100));
        fs::set_permissions(record_path(&root), fs::Permissions::from_mode(0o644)).unwrap();
        let bad_permissions = stop_owned(&root, Duration::from_millis(100));
        let survived = child.try_wait().unwrap().is_none();
        let _ = child.kill();
        let _ = child.wait();
        fs::remove_dir_all(root).unwrap();
        assert!(
            bad_inode
                .unwrap_err()
                .to_string()
                .contains("identity no longer matches")
        );
        assert!(
            bad_legacy_path
                .unwrap_err()
                .to_string()
                .contains("identity no longer matches")
        );
        assert!(
            bad_permissions
                .unwrap_err()
                .to_string()
                .contains("mode 0600")
        );
        assert!(survived, "unowned process was signalled");
    }
}
