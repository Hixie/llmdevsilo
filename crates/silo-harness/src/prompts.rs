//! Default system prompt. The text is stable for a given configuration
//! (no timestamps or random content), so deterministic sessions journal
//! identical requests.

use silo_core::sandbox::AccessReport;

pub(crate) fn default_system_prompt(report: &AccessReport, tool_names: &[String]) -> String {
    let readable = if report.readable_paths.is_empty() {
        "(none)".to_string()
    } else {
        report.readable_paths.join(", ")
    };
    let domains = if report.allowed_domains.is_empty() {
        "(none)".to_string()
    } else {
        report.allowed_domains.join(", ")
    };
    format!(
        "You are a software engineering agent working inside a sandbox.\n\
         Sandbox backend: {kind}\n\
         Workspace (read/write): {workspace}\n\
         Scratch space (writable, temporary): {scratch}\n\
         Readable host paths: {readable}\n\
         Network egress is limited to these domains: {domains}\n\
         Available tools: {tools}\n\
         Files uploaded by the user appear under _uploads/ in the workspace.\n\
         Use the Agent tool to delegate self-contained subtasks to subagents.",
        kind = report.sandbox_kind,
        workspace = report.workspace_mount,
        scratch = report.scratch_dir,
        readable = readable,
        domains = domains,
        tools = tool_names.join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> AccessReport {
        AccessReport {
            sandbox_kind: "mock".into(),
            workspace_mount: "/mnt/ws".into(),
            scratch_dir: "/scratch".into(),
            readable_paths: vec!["/usr/bin".into()],
            allowed_domains: vec!["crates.io".into()],
            credential_domains: vec![],
            notes: vec![],
        }
    }

    #[test]
    fn prompt_mentions_paths_tools_and_domains() {
        let prompt = default_system_prompt(&report(), &["Read".into(), "Bash".into()]);
        assert!(prompt.contains("/mnt/ws"));
        assert!(prompt.contains("/scratch"));
        assert!(prompt.contains("/usr/bin"));
        assert!(prompt.contains("crates.io"));
        assert!(prompt.contains("Read, Bash"));
        assert!(prompt.contains("mock"));
    }

    #[test]
    fn prompt_is_stable_for_the_same_inputs() {
        let tools = vec!["Read".to_string()];
        assert_eq!(
            default_system_prompt(&report(), &tools),
            default_system_prompt(&report(), &tools)
        );
    }

    #[test]
    fn empty_lists_render_as_none() {
        let empty = AccessReport::default();
        let prompt = default_system_prompt(&empty, &[]);
        assert!(prompt.contains("Readable host paths: (none)"));
        assert!(prompt.contains("domains: (none)"));
    }
}
