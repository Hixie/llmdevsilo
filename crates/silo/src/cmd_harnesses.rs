//! `silo harnesses list`: live harness discovery via the run files.

use std::path::Path;
use std::process::Command;

use silo_core::protocol::RunInfo;

pub fn list() -> anyhow::Result<u8> {
    let runs_dir = silo_core::paths::runs_dir(&silo_core::paths::state_dir());
    let entries = match std::fs::read_dir(&runs_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("no running harnesses");
            return Ok(0);
        }
        Err(error) => return Err(error.into()),
    };

    let mut rows: Vec<RunInfo> = Vec::new();
    let mut pruned = 0usize;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let info: Option<RunInfo> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        match info {
            Some(info) if pid_alive(info.pid) => rows.push(info),
            _ => {
                // Dead or unreadable: prune the run file.
                let _ = std::fs::remove_file(&path);
                pruned += 1;
            }
        }
    }

    if rows.is_empty() {
        println!("no running harnesses");
    } else {
        println!("{:<14} {:>8} {:<22} WORKSPACE", "HARNESS", "PID", "ADDRESS");
        for info in &rows {
            println!(
                "{:<14} {:>8} {:<22} {}",
                info.harness_id, info.pid, info.addr, info.workspace
            );
        }
    }
    if pruned > 0 {
        println!(
            "pruned {pruned} stale run file(s) from {}",
            display(&runs_dir)
        );
    }
    Ok(0)
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

/// Checks process liveness with `kill -0`.
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_process_is_alive() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn an_unlikely_pid_is_dead() {
        // Pid numbers near the macOS/Linux defaults' upper bound are very
        // unlikely to be in use.
        assert!(!pid_alive(99_999_999));
    }
}
