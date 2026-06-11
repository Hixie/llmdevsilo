//! `silo workspace lock|unlock|status`.

use std::path::Path;

use silo_workspace::{UnlockReport, WorkspaceManager};

use crate::cli::WorkspaceAction;

pub fn execute(action: WorkspaceAction) -> anyhow::Result<u8> {
    let manager = WorkspaceManager::new(silo_core::paths::state_dir());
    match action {
        WorkspaceAction::Lock { path } => {
            let status = manager.lock(&path)?;
            println!("Locked {}", status.path.display());
            for warning in &status.warnings {
                println!("warning: {warning}");
            }
            Ok(0)
        }
        WorkspaceAction::Unlock { path } => {
            let report = manager.unlock(&path)?;
            print_unlock_report(&path, &report);
            Ok(0)
        }
        WorkspaceAction::Status { path } => {
            let status = manager.status(&path)?;
            println!("workspace: {}", status.path.display());
            println!("locked: {}", if status.locked { "yes" } else { "no" });
            match &status.attached_harness {
                Some(id) => println!("attached: harness {id}"),
                None => println!("attached: no"),
            }
            println!("shells: {}", status.live_shells);
            for warning in &status.warnings {
                println!("warning: {warning}");
            }
            Ok(0)
        }
    }
}

fn print_unlock_report(path: &Path, report: &UnlockReport) {
    println!("Unlocked {}", path.display());
    println!();

    // Auto-exec surfaces come first: changes here can run code outside the
    // sandbox once the user touches the workspace.
    println!("==============================");
    println!("    AUTO-EXEC WARNINGS");
    println!("==============================");
    if report.auto_exec_flags.is_empty() {
        println!("(no changed auto-exec surfaces)");
    } else {
        println!("Review these files before opening the workspace in any tool:");
        for flag in &report.auto_exec_flags {
            println!("  !! {} — {}", flag.path, flag.reason);
        }
    }
    println!();

    println!("Changes since lock: {}", report.changes.len());
    if !report.changes.is_empty() {
        for change in &report.changes {
            let note = change
                .note
                .as_deref()
                .map(|note| format!(" ({note})"))
                .unwrap_or_default();
            println!(
                "  {:<8} {}{note}",
                format!("{:?}", change.kind).to_lowercase(),
                change.path
            );
        }
        println!();
        for change in &report.changes {
            if let Some(diff) = &change.diff {
                println!("--- {} ({:?}) ---", change.path, change.kind);
                println!("{diff}");
            }
        }
    }
}
