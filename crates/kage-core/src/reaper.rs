//! Reaping of orphaned grandchildren for long-lived processes.
//!
//! A kage daemon lives inside a PID namespace where it is PID 1, so
//! every process a finished tool call left behind reparents to the
//! daemon once its own parent exits. Nothing waits on those orphans,
//! and each unreaped one pins a PID until the daemon exits. On a busy
//! daemon that bleeds the namespace's PIDs dry. [`start`] installs a
//! background thread that collects them.
//!
//! The reaper cannot know which children a caller still intends to
//! wait on, so every status it collects is recorded, and the
//! [`try_wait`] and [`wait`] wrappers fall back to that record
//! when the kernel reports `ECHILD` because the reaper won the
//! race to a tracked child.

use std::io;
use std::process::{Child, ExitStatus};

pub use imp::{drain, start, try_wait, wait};

#[cfg(unix)]
mod imp {
    use super::{Child, ExitStatus, io};
    use std::collections::HashMap;
    use std::os::unix::process::ExitStatusExt;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

    /// How often the reaper looks for finished orphans.
    const PERIOD: Duration = Duration::from_millis(250);
    /// How long a recorded status is kept for a late waiter.
    const RETAIN: Duration = Duration::from_secs(300);
    /// How long a waiter whose child was just reaped waits for the
    /// status to become visible in the record.
    const GRACE: Duration = Duration::from_millis(100);

    #[derive(Clone, Copy)]
    struct Record {
        raw: i32,
        at: Instant,
    }

    static RECORDS: OnceLock<Mutex<HashMap<u32, Record>>> = OnceLock::new();
    static STARTED: OnceLock<()> = OnceLock::new();

    fn records() -> &'static Mutex<HashMap<u32, Record>> {
        RECORDS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Start the background reaper once per process; later calls do
    /// nothing.
    pub fn start() {
        STARTED.get_or_init(|| {
            let spawned = std::thread::Builder::new()
                .name("kage-reaper".to_owned())
                .spawn(|| {
                    loop {
                        drain();
                        std::thread::sleep(PERIOD);
                    }
                });
            if spawned.is_err() {
                // The daemon still works without the reaper; it only
                // leaks orphan PIDs again.
                eprintln!("kage: could not start the orphan reaper thread");
            }
        });
    }

    /// Reap every finished child, recording each status. Public so
    /// tests can drive a reaping pass by hand.
    pub fn drain() {
        while reap_one() {}
        let now = Instant::now();
        records()
            .lock()
            .unwrap()
            .retain(|_, record| now.duration_since(record.at) < RETAIN);
    }

    fn reap_one() -> bool {
        let (pid, raw) = match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, code)) => (pid, code << 8),
            Ok(WaitStatus::Signaled(pid, signal, core)) => {
                (pid, (signal as i32) | if core { 0x80 } else { 0 })
            }
            Ok(_) | Err(_) => return false,
        };
        let Ok(pid) = u32::try_from(pid.as_raw()) else {
            return false;
        };
        records().lock().unwrap().insert(
            pid,
            Record {
                raw,
                at: Instant::now(),
            },
        );
        true
    }

    /// The recorded status for `pid`, waiting out the moment between
    /// the reaper reaping the child and recording it.
    fn recorded(pid: u32) -> Option<i32> {
        let deadline = Instant::now() + GRACE;
        loop {
            if let Some(record) = records().lock().unwrap().remove(&pid) {
                return Some(record.raw);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Like [`Child::try_wait`], but a status the reaper already
    /// collected is still reported.
    pub fn try_wait(child: &mut Child) -> io::Result<Option<ExitStatus>> {
        match child.try_wait() {
            Ok(result) => Ok(result),
            Err(err) if err.raw_os_error() == Some(libc::ECHILD) => {
                Ok(recorded(child.id()).map(ExitStatus::from_raw))
            }
            Err(err) => Err(err),
        }
    }

    /// Like [`Child::wait`], but a status the reaper already collected
    /// is still reported.
    pub fn wait(child: &mut Child) -> io::Result<ExitStatus> {
        match child.wait() {
            Ok(status) => Ok(status),
            Err(err) if err.raw_os_error() == Some(libc::ECHILD) => match recorded(child.id()) {
                Some(raw) => Ok(ExitStatus::from_raw(raw)),
                None => Err(err),
            },
            Err(err) => Err(err),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::process::Command;

        /// The process state letter from `/proc/pid/stat`, or `None`
        /// once the process is fully gone.
        fn state(pid: u32) -> Option<String> {
            let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let after = text.rsplit(')').next()?;
            after.split_whitespace().next().map(str::to_owned)
        }

        /// Drive draining passes until the child `pid` has been
        /// reaped and its status recorded, which is the moment the
        /// kernel starts reporting `ECHILD` to its waiter. A drain
        /// pass from another test may do the reaping; it records the
        /// status all the same.
        fn drain_until_reaped(pid: u32) {
            for _ in 0..100 {
                drain();
                if records().lock().unwrap().contains_key(&pid) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            panic!("pid {pid} was never reaped and recorded");
        }

        #[test]
        #[allow(clippy::zombie_processes)]
        fn drain_reaps_a_finished_child() {
            let child = Command::new("true").spawn().unwrap();
            let pid = child.id();
            drain_until_reaped(pid);
            assert_eq!(state(pid), None, "zombie survived the drain");
        }

        #[test]
        fn wait_recovers_a_stolen_status() {
            let mut child = Command::new("true").spawn().unwrap();
            let pid = child.id();
            drain_until_reaped(pid);
            let status = wait(&mut child).expect("wait after the reaper stole the status");
            assert!(status.success());
        }

        #[test]
        fn try_wait_recovers_a_stolen_status() {
            let mut child = Command::new("true").spawn().unwrap();
            let pid = child.id();
            drain_until_reaped(pid);
            let status = try_wait(&mut child)
                .expect("try_wait after the reaper stole the status")
                .expect("child already exited");
            assert!(status.success());
        }

        #[test]
        fn wait_recovers_a_signaled_status() {
            let mut child = Command::new("sh")
                .arg("-c")
                .arg("kill -TERM $$")
                .spawn()
                .unwrap();
            let pid = child.id();
            drain_until_reaped(pid);
            let status = wait(&mut child).expect("wait after the reaper stole the status");
            assert_eq!(status.signal(), Some(15));
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{Child, ExitStatus, io};

    /// Nothing to start: only Unix reaps orphaned children.
    pub fn start() {}

    /// Nothing to drain: only Unix reaps orphaned children.
    pub fn drain() {}

    /// [`Child::try_wait`], as no reaper takes statuses here.
    pub fn try_wait(child: &mut Child) -> io::Result<Option<ExitStatus>> {
        child.try_wait()
    }

    /// [`Child::wait`], as no reaper takes statuses here.
    pub fn wait(child: &mut Child) -> io::Result<ExitStatus> {
        child.wait()
    }
}
