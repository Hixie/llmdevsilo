//! SBPL (Seatbelt) profile generation for the sandbox-exec backend.
//!
//! The profile starts from `(deny default)` and allows exactly: reading the
//! workspace, the scratch space, the configured allowlist, and the
//! operating-system baseline; writing the workspace and the scratch space;
//! loopback networking; Unix-socket connections inside the scratch space;
//! and the process, sysctl, and Mach services that libSystem needs to run
//! ordinary command-line programs. Generation is pure: it takes paths in
//! and produces the profile text, with no filesystem access.
//!
//! Callers pass canonicalized paths. Seatbelt evaluates path filters
//! against fully resolved paths, so a profile built from symlinked paths
//! (for example `/tmp/...` instead of `/private/tmp/...`) does not match.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use silo_core::error::SandboxError;

/// Directories every sandboxed process can read: dynamic linker data,
/// system libraries and binaries, certificates and other `/etc` data, and
/// timezone data. Read-only; nothing here is user-specific.
pub const BASELINE_READ_SUBPATHS: &[&str] = &[
    "/usr/lib",
    "/usr/share",
    "/usr/bin",
    "/bin",
    "/usr/sbin",
    "/sbin",
    "/System",
    "/Library/Preferences/Logging",
    "/private/etc",
    "/private/var/db/dyld",
    "/private/var/db/timezone",
];

/// Individual files and devices every sandboxed process can read. The
/// root directory itself is required: processes read `/` while resolving
/// paths and the subpath rules do not cover it.
pub const BASELINE_READ_LITERALS: &[&str] = &[
    "/",
    "/dev/null",
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/dev/dtracehelper",
];

/// Mach services libSystem contacts during normal process startup and
/// library use: user/group lookups, system configuration, notifications,
/// logging, and Launch Services.
const MACH_LOOKUP_NAMES: &[&str] = &[
    "com.apple.system.opendirectoryd.libinfo",
    "com.apple.SystemConfiguration.configd",
    "com.apple.system.notification_center",
    "com.apple.logd",
    "com.apple.coreservices.launchservicesd",
];

/// Inputs for one profile. All paths must be absolute and canonical.
#[derive(Clone, Debug, Default)]
pub struct ProfileSpec {
    /// Read/write workspace directory.
    pub workspace: PathBuf,
    /// Read/write per-sandbox scratch directory.
    pub scratch: PathBuf,
    /// Read-only host directories from the user-configured allowlist.
    pub read_allowlist: Vec<PathBuf>,
    /// Extra individual files readable inside the sandbox (the helper
    /// binary, which lives outside the workspace and the allowlist).
    pub read_files: Vec<PathBuf>,
}

/// The host paths a profile makes readable beyond the workspace and
/// scratch: the allowlist entries followed by the OS baseline. Used for
/// the access report.
pub fn readable_paths(read_allowlist: &[PathBuf]) -> Vec<String> {
    let mut paths: Vec<String> = read_allowlist
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    paths.extend(BASELINE_READ_SUBPATHS.iter().map(|s| s.to_string()));
    paths
}

/// Renders `path` as an SBPL string literal. Rejects paths containing a
/// double quote or a newline; escapes backslashes.
fn sbpl_string(path: &Path) -> Result<String, SandboxError> {
    let text = path.to_str().ok_or_else(|| {
        SandboxError::Setup(format!(
            "path {} is not valid UTF-8 and cannot appear in a sandbox profile",
            path.display()
        ))
    })?;
    if text.contains('"') || text.contains('\n') {
        return Err(SandboxError::Setup(format!(
            "path {text:?} contains a quote or newline and cannot appear in a sandbox profile"
        )));
    }
    Ok(format!("\"{}\"", text.replace('\\', "\\\\")))
}

fn require_absolute(path: &Path, role: &str) -> Result<(), SandboxError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(SandboxError::Setup(format!(
            "{role} path {} must be absolute",
            path.display()
        )))
    }
}

/// Generates the SBPL profile text for `spec`.
pub fn generate(spec: &ProfileSpec) -> Result<String, SandboxError> {
    require_absolute(&spec.workspace, "workspace")?;
    require_absolute(&spec.scratch, "scratch")?;
    for path in &spec.read_allowlist {
        require_absolute(path, "read allowlist")?;
    }
    for path in &spec.read_files {
        require_absolute(path, "read file")?;
    }

    let workspace = sbpl_string(&spec.workspace)?;
    let scratch = sbpl_string(&spec.scratch)?;

    // Sorted sets keep the output deterministic and drop duplicates.
    let mut read_subpaths = BTreeSet::new();
    for path in BASELINE_READ_SUBPATHS {
        read_subpaths.insert(sbpl_string(Path::new(path))?);
    }
    for path in &spec.read_allowlist {
        read_subpaths.insert(sbpl_string(path)?);
    }
    read_subpaths.insert(workspace.clone());
    read_subpaths.insert(scratch.clone());

    let mut read_literals = BTreeSet::new();
    for path in BASELINE_READ_LITERALS {
        read_literals.insert(sbpl_string(Path::new(path))?);
    }
    for path in &spec.read_files {
        read_literals.insert(sbpl_string(path)?);
    }

    let mut out = String::new();
    out.push_str("(version 1)\n");
    out.push_str("(deny default)\n");
    out.push_str("(allow process-fork)\n");
    out.push_str("(allow process-exec*)\n");
    // Metadata is readable everywhere so path traversal works. This leaks
    // the existence and attributes of files outside the allowlist; the
    // access report carries a note about it.
    out.push_str("(allow file-read-metadata)\n");
    out.push_str("(allow sysctl-read)\n");
    out.push_str("(allow signal (target same-sandbox))\n");

    out.push_str("(allow file-read*\n");
    for literal in &read_literals {
        out.push_str(&format!("  (literal {literal})\n"));
    }
    for subpath in &read_subpaths {
        out.push_str(&format!("  (subpath {subpath})\n"));
    }
    out.push_str(")\n");

    out.push_str(&format!(
        "(allow file-write*\n  (literal \"/dev/null\")\n  (subpath {workspace})\n  (subpath {scratch})\n)\n"
    ));
    out.push_str("(allow file-write-data (literal \"/dev/dtracehelper\"))\n");

    // Terminal devices: interactive user shells read and write the
    // controlling terminal and any pseudo-terminals they allocate.
    out.push_str("(allow file-read* file-write*\n  (literal \"/dev/tty\")\n  (regex #\"^/dev/ttys[0-9]\")\n)\n");
    out.push_str(
        "(allow file-ioctl\n  (literal \"/dev/dtracehelper\")\n  (regex #\"^/dev/tty\")\n)\n",
    );

    out.push_str("(allow mach-lookup\n");
    for name in MACH_LOOKUP_NAMES {
        out.push_str(&format!("  (global-name \"{name}\")\n"));
    }
    out.push_str(")\n");

    // Network policy: loopback only, plus Unix-socket connections inside
    // the scratch space (the helper control socket). All other egress is
    // denied; HTTP(S) leaves through the proxy on loopback.
    out.push_str("(allow network-outbound (remote ip \"localhost:*\"))\n");
    out.push_str(&format!("(allow network-outbound (subpath {scratch}))\n"));
    out.push_str("(allow network-inbound (local ip \"localhost:*\"))\n");
    out.push_str("(allow network-bind (local ip \"localhost:*\"))\n");

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ProfileSpec {
        ProfileSpec {
            workspace: PathBuf::from("/private/tmp/ws"),
            scratch: PathBuf::from("/private/tmp/scratch"),
            read_allowlist: vec![PathBuf::from("/opt/tools"), PathBuf::from("/usr/local")],
            read_files: vec![PathBuf::from("/build/silo-helper")],
        }
    }

    #[test]
    fn profile_has_deny_default_and_core_allows() {
        let text = generate(&spec()).unwrap();
        assert!(text.starts_with("(version 1)\n(deny default)\n"));
        for clause in [
            "(allow process-fork)",
            "(allow process-exec*)",
            "(allow file-read-metadata)",
            "(allow sysctl-read)",
            "(allow signal (target same-sandbox))",
        ] {
            assert!(text.contains(clause), "missing {clause} in:\n{text}");
        }
    }

    #[test]
    fn reads_cover_workspace_scratch_allowlist_and_baseline() {
        let text = generate(&spec()).unwrap();
        for subpath in [
            "/private/tmp/ws",
            "/private/tmp/scratch",
            "/opt/tools",
            "/usr/local",
        ] {
            assert!(
                text.contains(&format!("(subpath \"{subpath}\")")),
                "missing read subpath {subpath}"
            );
        }
        for subpath in BASELINE_READ_SUBPATHS {
            assert!(
                text.contains(&format!("(subpath \"{subpath}\")")),
                "missing baseline subpath {subpath}"
            );
        }
        for literal in BASELINE_READ_LITERALS {
            assert!(
                text.contains(&format!("(literal \"{literal}\")")),
                "missing baseline literal {literal}"
            );
        }
        assert!(text.contains("(literal \"/build/silo-helper\")"));
    }

    #[test]
    fn writes_cover_only_workspace_scratch_and_devices() {
        let text = generate(&spec()).unwrap();
        let write_section = text
            .split("(allow file-write*\n")
            .nth(1)
            .unwrap()
            .split(')')
            .collect::<Vec<_>>()
            .join(")");
        let _ = write_section;
        assert!(text.contains(
            "(allow file-write*\n  (literal \"/dev/null\")\n  (subpath \"/private/tmp/ws\")\n  (subpath \"/private/tmp/scratch\")\n)"
        ));
        assert!(text.contains("(allow file-write-data (literal \"/dev/dtracehelper\"))"));
        // The allowlist never appears in a write rule.
        assert!(!text.contains("file-write*\n  (subpath \"/opt/tools\")"));
    }

    #[test]
    fn network_rules_are_loopback_and_scratch_unix_sockets_only() {
        let text = generate(&spec()).unwrap();
        assert!(text.contains("(allow network-outbound (remote ip \"localhost:*\"))"));
        assert!(text.contains("(allow network-inbound (local ip \"localhost:*\"))"));
        assert!(text.contains("(allow network-bind (local ip \"localhost:*\"))"));
        assert!(text.contains("(allow network-outbound (subpath \"/private/tmp/scratch\"))"));
        assert!(!text.contains("(allow network*"));
    }

    #[test]
    fn mach_lookup_list_is_exactly_the_baseline() {
        let text = generate(&spec()).unwrap();
        for name in MACH_LOOKUP_NAMES {
            assert!(text.contains(&format!("(global-name \"{name}\")")));
        }
        assert_eq!(
            text.matches("(global-name ").count(),
            MACH_LOOKUP_NAMES.len()
        );
    }

    #[test]
    fn quote_and_newline_paths_are_rejected() {
        let mut bad = spec();
        bad.workspace = PathBuf::from("/tmp/has\"quote");
        assert!(matches!(generate(&bad), Err(SandboxError::Setup(_))));

        let mut bad = spec();
        bad.read_allowlist = vec![PathBuf::from("/tmp/has\nnewline")];
        assert!(matches!(generate(&bad), Err(SandboxError::Setup(_))));
    }

    #[test]
    fn backslashes_are_escaped() {
        let mut s = spec();
        s.read_allowlist = vec![PathBuf::from("/odd/back\\slash")];
        let text = generate(&s).unwrap();
        assert!(text.contains("(subpath \"/odd/back\\\\slash\")"));
    }

    #[test]
    fn relative_paths_are_rejected() {
        let mut bad = spec();
        bad.scratch = PathBuf::from("relative/scratch");
        assert!(matches!(generate(&bad), Err(SandboxError::Setup(_))));
        let mut bad = spec();
        bad.read_files = vec![PathBuf::from("silo-helper")];
        assert!(matches!(generate(&bad), Err(SandboxError::Setup(_))));
    }

    #[test]
    fn duplicate_paths_collapse_and_output_is_deterministic() {
        let mut s = spec();
        s.read_allowlist = vec![
            PathBuf::from("/opt/tools"),
            PathBuf::from("/opt/tools"),
            PathBuf::from("/usr/bin"),
        ];
        let first = generate(&s).unwrap();
        assert_eq!(first.matches("(subpath \"/opt/tools\")").count(), 1);
        assert_eq!(first.matches("(subpath \"/usr/bin\")").count(), 1);
        s.read_allowlist.reverse();
        let second = generate(&s).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn readable_paths_lists_allowlist_then_baseline() {
        let paths = readable_paths(&[PathBuf::from("/opt/tools")]);
        assert_eq!(paths[0], "/opt/tools");
        assert!(paths.contains(&"/usr/lib".to_string()));
        assert_eq!(paths.len(), 1 + BASELINE_READ_SUBPATHS.len());
    }
}
