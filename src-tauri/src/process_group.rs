//! Child processes in their own process group (ENG-1527).
//!
//! Local coding CLIs (Claude Code / Codex) spawn their own children (shells,
//! test runners). Killing only the direct child leaks that subtree — both CLIs
//! are known to orphan children to PID 1 when their parent dies. Spawning the
//! CLI in its OWN process group lets cancel/quit signal the whole tree at once.
//!
//! Unix: real process groups (`setpgid` via `process_group(0)`) + `killpg`.
//! Windows: Job Objects own the CLI process tree and terminate all descendants
//! together on cancel, quit, or last-handle close.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::sync::Arc;
use std::sync::Mutex;

use tokio::process::{Child, Command};

/// A child running in its own process group.
pub struct GroupChild {
    child: Child,
    #[cfg(unix)]
    pgid: i32,
    #[cfg(windows)]
    job: Arc<WindowsJob>,
}

#[cfg(windows)]
struct WindowsJob {
    handle: usize,
}

#[cfg(windows)]
impl WindowsJob {
    fn attach(child: &Child) -> std::io::Result<Self> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }

        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if configured == 0 {
            let error = std::io::Error::last_os_error();
            unsafe { CloseHandle(handle) };
            return Err(error);
        }

        let process = match child.raw_handle() {
            Some(process) => process,
            None => {
                unsafe { CloseHandle(handle) };
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "child exited before assignment to a Windows Job Object",
                ));
            }
        };
        if unsafe { AssignProcessToJobObject(handle, process.cast()) } == 0 {
            let error = std::io::Error::last_os_error();
            unsafe { CloseHandle(handle) };
            return Err(error);
        }

        Ok(Self {
            handle: handle as usize,
        })
    }

    fn terminate(&self) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        unsafe {
            TerminateJobObject(self.handle as _, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        unsafe { CloseHandle(self.handle as _) };
    }
}

impl GroupChild {
    /// Spawn `cmd` in a fresh process group.
    pub fn spawn(cmd: &mut Command) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            // 0 = "same as the child's pid": the child leads a new group.
            cmd.as_std_mut().process_group(0);
        }
        let mut child = cmd.spawn()?;
        #[cfg(unix)]
        let pgid = child.id().map(|id| id as i32).unwrap_or(-1);
        #[cfg(windows)]
        let job = match WindowsJob::attach(&child) {
            Ok(job) => Arc::new(job),
            Err(error) => {
                // Do not return a live, uncontained coding CLI if process-tree
                // ownership could not be established.
                let _ = child.start_kill();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            #[cfg(unix)]
            pgid,
            #[cfg(windows)]
            job,
        })
    }

    /// Graceful cancel: SIGINT to the whole group — both CLIs treat SIGINT as
    /// "interrupt cleanly" (session state stays resumable).
    pub fn interrupt(&mut self) {
        self.signal_group(libc_signal::SIGINT);
    }

    /// Firm stop: SIGTERM to the whole group.
    pub fn terminate(&mut self) {
        self.signal_group(libc_signal::SIGTERM);
    }

    /// Last resort: SIGKILL to the whole group.
    pub fn kill_group(&mut self) {
        self.signal_group(libc_signal::SIGKILL);
    }

    // Takes `&mut self` for the Windows arm's sake: `Child::start_kill` needs a
    // mutable receiver. Unix does not need it (killpg goes through the stored
    // pgid), but the signature is shared, so both arms take it.
    #[cfg(unix)]
    fn signal_group(&mut self, sig: i32) {
        if self.pgid > 0 {
            // SAFETY: killpg with a validated positive pgid; failure (already
            // exited) is benign and reported by errno, which we ignore.
            unsafe {
                libc::killpg(self.pgid, sig);
            }
        }
    }

    #[cfg(windows)]
    fn signal_group(&mut self, _sig: i32) {
        // A background GUI app cannot deliver POSIX-style console signals.
        // Terminate the Job Object so the CLI and every descendant exit as one
        // contained tree; wait() below still reaps the direct child.
        self.job.terminate();
    }

    #[cfg(not(any(unix, windows)))]
    fn signal_group(&mut self, _sig: i32) {
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
    #[cfg(unix)]
    groups: Mutex<HashMap<String, i32>>,
    #[cfg(windows)]
    groups: Mutex<HashMap<String, Arc<WindowsJob>>>,
    #[cfg(not(any(unix, windows)))]
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

    #[cfg(windows)]
    pub fn register(&self, request_id: &str, child: &GroupChild) {
        self.groups
            .lock()
            .expect("process registry lock poisoned")
            .insert(request_id.to_string(), Arc::clone(&child.job));
    }

    #[cfg(not(any(unix, windows)))]
    pub fn register(&self, _request_id: &str, _child: &GroupChild) {}

    pub fn unregister(&self, request_id: &str) {
        #[cfg(unix)]
        self.groups
            .lock()
            .expect("process registry lock poisoned")
            .remove(request_id);
        #[cfg(windows)]
        self.groups
            .lock()
            .expect("process registry lock poisoned")
            .remove(request_id);
        #[cfg(not(any(unix, windows)))]
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
            let groups = self.groups.lock().expect("process registry lock poisoned");
            for pgid in groups.values() {
                if *pgid > 0 {
                    // SAFETY: killpg with validated positive pgid (see above).
                    unsafe {
                        libc::killpg(*pgid, libc::SIGTERM);
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            let groups = self.groups.lock().expect("process registry lock poisoned");
            for job in groups.values() {
                job.terminate();
            }
        }
    }

    #[allow(dead_code)] // consumed by ENG-1528 (coding-run adapters)
    pub fn live_count(&self) -> usize {
        #[cfg(any(unix, windows))]
        {
            self.groups
                .lock()
                .expect("process registry lock poisoned")
                .len()
        }
        #[cfg(not(any(unix, windows)))]
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    fn process_is_running(pid: u32) -> bool {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return false;
        }
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        unsafe {
            CloseHandle(handle);
        }
        wait == WAIT_TIMEOUT
    }

    #[tokio::test]
    async fn interrupt_terminates_the_windows_child_tree() {
        let script = "$child = Start-Process ping.exe -ArgumentList @('127.0.0.1','-n','60') -WindowStyle Hidden -PassThru; [Console]::WriteLine($child.Id); [Console]::Out.Flush(); Wait-Process -Id $child.Id";
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoLogo", "-NoProfile", "-Command", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = GroupChild::spawn(&mut command).expect("spawn parent and descendant");
        let stdout = child.stdout_take().expect("parent stdout");
        let mut lines = BufReader::new(stdout).lines();
        let descendant_pid: u32 = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
            .await
            .expect("parent reports descendant promptly")
            .expect("stdout read succeeds")
            .expect("descendant pid line exists")
            .trim()
            .parse()
            .expect("descendant pid is numeric");
        assert!(process_is_running(descendant_pid));

        child.interrupt();
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("parent exits promptly")
            .expect("wait succeeds");
        tokio::time::sleep(Duration::from_millis(250)).await;

        let survived = process_is_running(descendant_pid);
        if survived {
            let _ = std::process::Command::new("taskkill.exe")
                .args(["/PID", &descendant_pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        assert!(
            !survived,
            "descendant process {descendant_pid} was orphaned"
        );
    }

    #[tokio::test]
    async fn registry_kill_all_terminates_the_windows_child_tree() {
        let script = "$child = Start-Process ping.exe -ArgumentList @('127.0.0.1','-n','60') -WindowStyle Hidden -PassThru; [Console]::WriteLine($child.Id); [Console]::Out.Flush(); Wait-Process -Id $child.Id";
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoLogo", "-NoProfile", "-Command", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = GroupChild::spawn(&mut command).expect("spawn parent and descendant");
        let stdout = child.stdout_take().expect("parent stdout");
        let mut lines = BufReader::new(stdout).lines();
        let descendant_pid: u32 = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
            .await
            .expect("parent reports descendant promptly")
            .expect("stdout read succeeds")
            .expect("descendant pid line exists")
            .trim()
            .parse()
            .expect("descendant pid is numeric");
        let registry = ProcessRegistry::new();
        registry.register("windows-tree", &child);
        assert_eq!(registry.live_count(), 1);

        registry.kill_all();
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("registered parent exits promptly")
            .expect("wait succeeds");
        tokio::time::sleep(Duration::from_millis(250)).await;
        registry.unregister("windows-tree");

        let survived = process_is_running(descendant_pid);
        if survived {
            let _ = std::process::Command::new("taskkill.exe")
                .args(["/PID", &descendant_pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        assert!(
            !survived,
            "descendant process {descendant_pid} survived kill_all"
        );
        assert_eq!(registry.live_count(), 0);
    }
}
