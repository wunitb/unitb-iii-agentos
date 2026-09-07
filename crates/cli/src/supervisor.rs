use anyhow::{Context, Result};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::signal::unix::{SignalKind, signal};

pub(crate) const MODE: &str = "__agentos_supervise_engine";

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
}

pub(crate) async fn run() -> Result<()> {
    let mut args = std::env::args_os().skip(2);
    let binary = PathBuf::from(args.next().context("Missing supervised engine binary")?);
    let config = PathBuf::from(args.next().context("Missing supervised engine config")?);
    anyhow::ensure!(args.next().is_none(), "Unexpected supervisor argument");
    crate::lifecycle::enable_child_subreaper()?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut engine = tokio::process::Command::new(binary)
        .arg("--config")
        .arg(config)
        .spawn()
        .context("Cannot start supervised engine")?;
    let status = tokio::select! {
        biased;
        _ = terminate.recv() => None,
        _ = interrupt.recv() => None,
        result = engine.wait() => Some(result?),
    };
    let stopping = status.is_none();
    let status = if let Some(status) = status {
        status
    } else {
        if let Some(pid) = engine.id() {
            // This unreaped Child is launch authority, not a PID-file lookup.
            unsafe {
                kill(i32::try_from(pid)?, 15);
            }
        }
        match tokio::time::timeout(Duration::from_secs(2), engine.wait()).await {
            Ok(result) => result?,
            Err(_) => {
                engine.start_kill()?;
                engine.wait().await?
            }
        }
    };
    reap_descendants(Duration::from_secs(1))?;
    anyhow::ensure!(
        stopping || status.success(),
        "Supervised engine exited: {status}"
    );
    Ok(())
}

fn direct_children() -> Result<BTreeSet<i32>> {
    let mut children = BTreeSet::new();
    for task in std::fs::read_dir("/proc/self/task")? {
        let task = match task {
            Ok(task) => task,
            Err(error) if crate::lifecycle::procfs_entry_gone(&error) => continue,
            Err(error) => return Err(error).context("Cannot inspect supervised task entry"),
        };
        let text = match std::fs::read_to_string(task.path().join("children")) {
            Ok(text) => text,
            Err(error) if crate::lifecycle::procfs_entry_gone(&error) => continue,
            Err(error) => return Err(error).context("Cannot inspect supervised children"),
        };
        for pid in text.split_whitespace() {
            let pid = pid.parse::<i32>()?;
            anyhow::ensure!(pid > 0, "Invalid supervised child pid");
            children.insert(pid);
        }
    }
    anyhow::ensure!(children.len() <= 1024, "Too many supervised children");
    Ok(children)
}

fn reap_exited() -> Result<()> {
    loop {
        let result = unsafe { waitpid(-1, std::ptr::null_mut(), 1) };
        if result == 0 {
            return Ok(());
        }
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(10) {
                return Ok(());
            }
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("Cannot reap supervised child");
            }
        }
    }
}

fn reap_descendants(grace: Duration) -> Result<()> {
    let deadline = Instant::now() + grace;
    let kill_deadline = deadline + Duration::from_secs(1);
    let mut notified = BTreeSet::new();
    loop {
        reap_exited()?;
        let children = direct_children()?;
        if children.is_empty() {
            return Ok(());
        }
        let force = Instant::now() >= deadline;
        for pid in children {
            if force || notified.insert(pid) {
                // Kernel parentage is authority. These are our direct, unreaped
                // children; their PIDs cannot be reused until we call waitpid.
                // Re-scan after reaping because a dying helper can orphan more.
                unsafe {
                    kill(pid, if force { 9 } else { 15 });
                }
            }
        }
        anyhow::ensure!(
            Instant::now() < kill_deadline,
            "Supervised descendants did not exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
