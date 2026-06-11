//! `silo run`: one harness session.

use anyhow::Context;
use tokio::sync::oneshot;

use silo_core::config::FrontendKind;
use silo_core::protocol::RunInfo;
use silo_core::replay::{SharedScript, TestScript};
use silo_harness::RunOptions;
use silo_workspace::{ContainerStrategy, WorkspaceManager};

use crate::cli::RunArgs;

pub async fn execute(args: RunArgs) -> anyhow::Result<u8> {
    let config = crate::cli::build_run_config(&args)?;
    for warning in crate::cli::startup_warnings(&config) {
        eprintln!("warning: {warning}");
    }
    let state_dir = silo_core::paths::state_dir();

    let script = match &args.script {
        Some(path) => {
            let script = TestScript::load(path)
                .with_context(|| format!("loading test script {}", path.display()))?;
            Some(SharedScript::new(script))
        }
        None => None,
    };

    if args.create {
        // Deterministic runs use the plain-directory strategy so locking
        // does not depend on platform disk-image tooling.
        let strategy = if args.deterministic {
            ContainerStrategy::PlainDir
        } else {
            ContainerStrategy::default_for_platform()
        };
        let status = WorkspaceManager::with_strategy(state_dir.clone(), strategy)
            .lock(&config.workspace)
            .with_context(|| format!("locking workspace {}", config.workspace.display()))?;
        for warning in &status.warnings {
            eprintln!("warning: {warning}");
        }
    }

    // For the interactive frontend, print connection details once the run
    // file exists.
    let notify_started = if config.frontend.kind == FrontendKind::Interactive {
        let (tx, rx) = oneshot::channel();
        let run_file =
            silo_core::paths::runs_dir(&state_dir).join(format!("{}.json", config.harness_id));
        tokio::spawn(async move {
            if rx.await.is_ok() {
                let info = std::fs::read_to_string(&run_file)
                    .ok()
                    .and_then(|text| serde_json::from_str::<RunInfo>(&text).ok());
                match info {
                    Some(info) => {
                        println!("Interactive frontend: wss://{}", info.addr);
                        println!(
                            "Certificate fingerprint (SHA-256): {}",
                            info.cert_fingerprint_sha256
                        );
                        println!("Run file: {}", run_file.display());
                    }
                    None => eprintln!("warning: run file {} is unreadable", run_file.display()),
                }
            }
        });
        Some(tx)
    } else {
        None
    };

    let headless = config.frontend.kind == FrontendKind::Headless;
    let options = RunOptions {
        script,
        deterministic: args.deterministic,
        mock_proxy: args.mock_proxy,
        allow_risky_paths: args.allow_risky_path.clone(),
        notify_started,
        ..RunOptions::default()
    };

    let outcome = silo_harness::run(config, options).await?;
    if let Some(failure) = &outcome.llm_failure {
        eprintln!("silo: session ended by LLM failure: {failure}");
    } else if !headless {
        // The headless frontend prints the final message itself.
        if let Some(message) = &outcome.message {
            println!("{message}");
        }
    }
    if let Some(path) = &outcome.journal_path {
        eprintln!("journal: {}", path.display());
    }
    Ok(exit_code(&outcome))
}

/// Maps the harness outcome to the process exit code: 3 when the session
/// was ended by consecutive LLM failures, 0 otherwise.
fn exit_code(outcome: &silo_harness::HarnessOutcome) -> u8 {
    if outcome.llm_failure.is_some() {
        3
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use silo_harness::HarnessOutcome;

    #[test]
    fn llm_failure_maps_to_exit_code_3() {
        let normal = HarnessOutcome {
            message: Some("done".into()),
            ..HarnessOutcome::default()
        };
        assert_eq!(exit_code(&normal), 0);

        let failed = HarnessOutcome {
            message: Some("quota exceeded".into()),
            llm_failure: Some("quota exceeded".into()),
            ..HarnessOutcome::default()
        };
        assert_eq!(exit_code(&failed), 3);
    }
}
