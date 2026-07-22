//! Child processes in their own process group (ENG-1527).
//!
//! Local coding CLIs (Claude Code / Codex) spawn their own children (shells,
//! test runners). Killing only the direct child leaks that subtree — both CLIs
//! are known to orphan children to PID 1 when their parent dies. Spawning the
//! CLI in its OWN process group lets cancel/quit signal the whole tree at once.
//!
//! Unix: real process groups (`setpgid` via `process_group(0)`) + `killpg`.
//! Windows: falls back to killing the direct child only (ENG-206 owns the
//! Windows story; Codex's own sandbox limits the blast radius there).

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::sync::Mutex;

use tokio::process::{Child, Command};

/// A child running in its own process group.
pub struct GroupChild {
    child: Child,
    #[cfg(unix)]
    pgid: i32,
}

impl GroupChild {
    /// Spawn `cmd` in a fresh process group.
    pub fn spawn(cmd: &mut Command) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            // 0 = "same as the child's pid": the child leads a new group.
            cmd.as_std_mut().process_group(0);
        }
        let child = cmd.spawn()?;
        #[cfg(unix)]
        let pgid = child.id().map(|id| id as i32).unwrap_or(-1);
        Ok(Self {
            child,
            #[cfg(unix)]
            pgid,
        })
    }

    /// Graceful cancel: SIGINT to the whole group — both CLIs treat SIGINT as
    /// "interrupt cleanly" (session state stays resumable).
    pub fn interrupt(&self) {
        self.signal_group(libc_signal::SIGINT);
    }

    /// Firm stop: SIGTERM to the whole group.
    pub fn terminate(&self) {
        self.signal_group(libc_signal::SIGTERM);
    }

    /// Last resort: SIGKILL to the whole group.
    pub fn kill_group(&self) {
        self.signal_group(libc_signal::SIGKILL);
    }

    #[cfg(unix)]
    fn signal_group(&self, sig: i32) {
        if self.pgid > 0 {
            // SAFETY: killpg with a validated positive pgid; failure (already
            // exited) is benign and reported by errno, which we ignore.
            unsafe {
                libc::killpg(self.pgid, sig);
            }
        }
    }

    #[cfg(not(unix))]
    fn signal_group(&self, _sig: i32) {
        // Windows: no process groups in this model — kill the direct child.
        // start_kill() is async-signal-safe here (does not require &mut self
        // polling); ignore failure if it already exited.
        let _ = self.child.start_kill();
    }

    /// Wait for the direct child to exit.
    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    pub fn stdout_take(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.stdout.take()
    }

    pub fn stderr_take(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.stderr.take()
    }

    #[allow(dead_code)] // Codex stdin-prompt path, ENG-1529
    pub fn stdin_take(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.stdin.take()
    }

    #[cfg(unix)]
    #[allow(dead_code)] // test accessor
    pub fn pgid(&self) -> i32 {
        self.pgid
    }
}

/// Signal numbers, kept in one place. On unix these come from libc; the
/// non-unix build only needs the constants to exist for the shared API.
mod libc_signal {
    #[cfg(unix)]
    pub use libc::{SIGINT, SIGKILL, SIGTERM};
    #[cfg(not(unix))]
    pub const SIGINT: i32 = 2;
    #[cfg(not(unix))]
    pub const SIGTERM: i32 = 15;
    #[cfg(not(unix))]
    pub const SIGKILL: i32 = 9;
}

/// Registry of live process groups so app shutdown can reap everything.
/// Sub-issue ENG-1528 registers each coding run here; the tray Quit handler
/// calls [`ProcessRegistry::kill_all`] before `app.exit(0)`.
#[derive(Default)]
pub struct ProcessRegistry {
    #[cfg_attr(not(unix), allow(dead_code))]
    groups: Mutex<HashMap<String, i32>>,
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(unix)]
    pub fn register(&self, request_id: &str, child: &GroupChild) {
        if child.pgid > 0 {
            self.groups
                .lock()
                .expect("process registry lock poisoned")
                .insert(request_id.to_string(), child.pgid);
        }
    }

    #[cfg(not(unix))]
    pub fn register(&self, _request_id: &str, _child: &GroupChild) {}

    pub fn unregister(&self, request_id: &str) {
        #[cfg(unix)]
        self.groups
            .lock()
            .expect("process registry lock poisoned")
            .remove(request_id);
        #[cfg(not(unix))]
        let _ = request_id;
    }

    /// SIGTERM every registered group. Sync on purpose: callable from the
    /// tray's non-async quit handler. Known limitation (recorded in the
    /// stream handoff): a Cmd+Q AppKit `terminate:` bypasses this path — an
    /// orphaned CLI finishes its current run and exits; its session file
    /// stays resumable.
    pub fn kill_all(&self) {
        #[cfg(unix)]
        {
            let groups = self
                .groups
                .lock()
                .expect("process registry lock poisoned");
            for pgid in groups.values() {
                if *pgid > 0 {
                    // SAFETY: killpg with validated positive pgid (see above).
                    unsafe {
                        libc::killpg(*pgid, libc::SIGTERM);
                    }
                }
            }
        }
    }

    #[allow(dead_code)] // consumed by ENG-1528 (coding-run adapters)
    pub fn live_count(&self) -> usize {
        #[cfg(unix)]
        {
            self.groups
                .lock()
                .expect("process registry lock poisoned")
                .len()
        }
        #[cfg(not(unix))]
        {
            0
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    fn sleeper(secs: u32) -> Command {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg(secs.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }

    #[tokio::test]
    async fn interrupt_stops_the_group_promptly() {
        let mut child = GroupChild::spawn(&mut sleeper(30)).expect("spawn sleep");
        assert!(child.pgid() > 0, "child must lead its own group");

        let start = Instant::now();
        child.interrupt();
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("interrupted child must exit well before its sleep")
            .expect("wait succeeds");
        assert!(!status.success(), "SIGINT exit is not a success status");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn registry_kill_all_terminates_registered_groups() {
        let registry = ProcessRegistry::new();
        let mut child = GroupChild::spawn(&mut sleeper(30)).expect("spawn sleep");
        registry.register("req-1", &child);
        assert_eq!(registry.live_count(), 1);

        registry.kill_all();
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("kill_all must terminate the group")
            .expect("wait succeeds");

        registry.unregister("req-1");
        assert_eq!(registry.live_count(), 0);
    }

    #[tokio::test]
    async fn unregister_then_kill_all_spares_the_process() {
        let registry = ProcessRegistry::new();
        let mut child = GroupChild::spawn(&mut sleeper(2)).expect("spawn sleep");
        registry.register("req-1", &child);
        registry.unregister("req-1");

        registry.kill_all();
        // The child must still be running (kill_all had nothing to do) and
        // then exit on its own schedule.
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("child exits on its own")
            .expect("wait succeeds");
        assert!(status.success(), "un-registered child must not be killed");
    }
}
