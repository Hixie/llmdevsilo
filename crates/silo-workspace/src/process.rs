//! Process liveness, identity, and termination probes.
//!
//! Attachment records carry a pid plus a start-time hint captured when the
//! attachment was made. A pid whose current start time differs from the
//! recorded hint belongs to a different (recycled) process and is treated
//! as dead; it is never signalled.

use std::process::Command;

/// Start-time identity hint for a pid: the `lstart` column from `ps`.
/// Empty when the process does not exist or `ps` is unavailable.
pub(crate) fn start_hint(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => String::new(),
    }
}

/// Compares a stored start-time hint with the pid's current one. An empty
/// stored hint or an unavailable current hint counts as a match; two
/// different non-empty values mean the pid was recycled.
pub(crate) fn identity_matches(pid: u32, stored_hint: &str) -> bool {
    if stored_hint.is_empty() {
        return true;
    }
    let current = start_hint(pid);
    current.is_empty() || current == stored_hint
}

/// Liveness probe via `kill(pid, 0)`. A permission error means the
/// process exists.
#[cfg(unix)]
pub(crate) fn alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    if pid == 0 {
        return false;
    }
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
pub(crate) fn alive(_pid: u32) -> bool {
    true
}

/// Liveness plus identity: the pid exists and its start time matches the
/// stored hint.
pub(crate) fn alive_with_hint(pid: u32, stored_hint: &str) -> bool {
    alive(pid) && identity_matches(pid, stored_hint)
}

/// True when the process exists and is not a zombie awaiting reaping.
pub(crate) fn running(pid: u32) -> bool {
    if !alive(pid) {
        return false;
    }
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "state="])
        .output();
    match output {
        Ok(output) if output.status.success() => !String::from_utf8_lossy(&output.stdout)
            .trim()
            .starts_with('Z'),
        _ => true,
    }
}

/// Pids that are never terminated or counted as unlock survivors: the
/// kernel/init pids and the current process.
pub(crate) fn is_protected(pid: u32) -> bool {
    pid == 0 || pid == 1 || pid == std::process::id()
}

/// Sends SIGTERM to an attached process, waits up to five seconds for it
/// to exit, then sends SIGKILL and waits up to two more seconds. A pid
/// whose identity hint no longer matches is never signalled. Errors are
/// ignored so a stale attachment never blocks an unlock. Protected pids
/// (0, 1, the current process) are never signalled.
#[cfg(unix)]
pub(crate) fn terminate(pid: u32, stored_hint: &str) {
    use nix::sys::signal::{kill, Signal};
    use nix::sys::wait::{waitpid, WaitPidFlag};
    use nix::unistd::Pid;
    use std::time::{Duration, Instant};

    if is_protected(pid) || !identity_matches(pid, stored_hint) {
        return;
    }
    let target = Pid::from_raw(pid as i32);
    if kill(target, Signal::SIGTERM).is_err() {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // Reaps the process when it is a child of this one, so the
        // existence check below sees it disappear.
        let _ = waitpid(target, Some(WaitPidFlag::WNOHANG));
        if kill(target, None).is_err() {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = kill(target, Signal::SIGKILL);
    let kill_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < kill_deadline {
        let _ = waitpid(target, Some(WaitPidFlag::WNOHANG));
        if !running(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(not(unix))]
pub(crate) fn terminate(_pid: u32, _stored_hint: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_process_probes_as_alive() {
        assert!(alive(std::process::id()));
        assert!(!alive(0));
    }

    #[test]
    fn an_empty_hint_skips_the_identity_check() {
        assert!(identity_matches(std::process::id(), ""));
        assert!(alive_with_hint(std::process::id(), ""));
    }

    #[test]
    fn the_identity_hint_detects_a_recycled_pid() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();

        let hint = start_hint(pid);
        assert!(!hint.is_empty(), "ps produced no start hint");
        assert!(identity_matches(pid, &hint));
        assert!(alive_with_hint(pid, &hint));

        // A stored hint from a different process generation: the pid is
        // treated as recycled and therefore dead.
        let tampered = "Mon Jan  1 00:00:00 2001";
        assert!(!identity_matches(pid, tampered));
        assert!(!alive_with_hint(pid, tampered));

        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn terminate_skips_a_recycled_pid_and_kills_a_matching_one() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let hint = start_hint(pid);

        // Mismatched identity: the process is left alone.
        terminate(pid, "Mon Jan  1 00:00:00 2001");
        assert!(alive(pid));

        // Matching identity: the process is terminated and reaped.
        terminate(pid, &hint);
        assert!(!alive_with_hint(pid, &hint));
        let _ = child.try_wait();
    }
}
