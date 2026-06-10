//! `silo replay-test`: convert a journal into a replayable test script.

use anyhow::Context;

use silo_core::journal::{read_journal, JournalEntry};
use silo_core::replay::script_from_journal;

use crate::cli::ReplayTestArgs;

pub fn execute(args: ReplayTestArgs) -> anyhow::Result<u8> {
    let records = read_journal(&args.journal)
        .with_context(|| format!("reading journal {}", args.journal.display()))?;
    let name = args.name.clone().unwrap_or_else(|| {
        args.journal
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "replay".to_string())
    });
    let script = script_from_journal(&records, &name);
    script
        .save(&args.output)
        .with_context(|| format!("writing script {}", args.output.display()))?;

    println!(
        "Wrote {} (llm turns: {}, tool execs: {}, frontend steps: {})",
        args.output.display(),
        script.llm.len(),
        script.tools.len(),
        script.frontend.len()
    );
    let workspace = workspace_from_records(&records).unwrap_or_else(|| "<workspace>".to_string());
    println!("Replay it with:");
    println!(
        "  silo run --workspace {workspace} --frontend mock --llm mock --sandbox mock \
         --mock-proxy --script {} --deterministic",
        args.output.display()
    );
    Ok(0)
}

/// Extracts the workspace path from the journal's Meta record, whose
/// summary has the form "workspace=<path> llm=...".
fn workspace_from_records(records: &[silo_core::journal::JournalRecord]) -> Option<String> {
    records.iter().find_map(|record| match &record.entry {
        JournalEntry::Meta { config_summary, .. } => {
            let rest = config_summary.strip_prefix("workspace=")?;
            let end = rest.find(" llm=")?;
            Some(rest[..end].to_string())
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use silo_core::journal::JournalRecord;

    use super::*;

    #[test]
    fn workspace_is_extracted_from_the_meta_summary() {
        let record = JournalRecord {
            seq: 0,
            time: silo_core::clock::Timestamp {
                logical: 0,
                wall_ms: None,
            },
            entry: JournalEntry::Meta {
                harness_id: "h".into(),
                harness_version: "0.1.0".into(),
                config_summary: "workspace=/tmp/ws llm=Mock/claude sandbox=Mock frontend=Mock"
                    .into(),
            },
        };
        assert_eq!(
            workspace_from_records(&[record]).as_deref(),
            Some("/tmp/ws")
        );
        assert_eq!(workspace_from_records(&[]), None);
    }
}
