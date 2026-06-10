//! OCI runtime specification generation for the gVisor backend.
//!
//! Generation is pure: it takes host paths and an environment and produces
//! the `config.json` value, with no filesystem access, so it is unit
//! tested without gVisor. The runtime (`super::gvisor`) writes the value
//! into a bundle directory and runs it with `runsc`.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Workspace mount point inside the sandbox.
pub const WORKSPACE_DEST: &str = "/workspace";

/// Scratch mount point inside the sandbox.
pub const SCRATCH_DEST: &str = "/scratch";

/// Helper binary path inside the sandbox (copied into the scratch space
/// by the runtime).
pub const HELPER_DEST: &str = "/scratch/bin/silo-helper";

/// Connect string the helper uses to reach the harness.
pub const HELPER_CONNECT: &str = "unix:/scratch/helper.sock";

/// Unix socket inside the scratch space whose connections the harness
/// forwards to the egress proxy.
pub const PROXY_SOCKET_DEST: &str = "/scratch/proxy.sock";

/// Loopback port inside the sandbox where the helper's relay listens and
/// forwards to [`PROXY_SOCKET_DEST`].
pub const IN_SANDBOX_PROXY_PORT: u16 = 3128;

/// `PATH` inside the sandbox.
pub const SANDBOX_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Host directories bind-mounted read-only when they exist: system
/// binaries, libraries, certificates, and alternatives links.
pub const OS_DIR_CANDIDATES: &[&str] = &[
    "/usr",
    "/bin",
    "/lib",
    "/lib64",
    "/etc/ssl",
    "/etc/alternatives",
];

/// Standard OCI hardening: kernel interfaces hidden from the sandbox.
const MASKED_PATHS: &[&str] = &[
    "/proc/acpi",
    "/proc/asound",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/sched_debug",
    "/proc/scsi",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/sys/firmware",
    "/sys/devices/virtual/powercap",
];

/// Standard OCI hardening: kernel interfaces visible but read-only.
const READONLY_PATHS: &[&str] = &[
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

/// The members of [`OS_DIR_CANDIDATES`] for which `exists` returns true.
pub fn default_os_dirs(exists: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    OS_DIR_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .filter(|path| exists(path))
        .collect()
}

/// Rewrites the host-path environment from
/// [`crate::scratch::ScratchSpace::sandbox_env`] into the in-sandbox view:
/// values under the host scratch root move to [`SCRATCH_DEST`], and the
/// relay and `PATH` variables are appended.
pub fn container_env(
    host_env: &[(String, String)],
    scratch_host_root: &Path,
) -> Vec<(String, String)> {
    let root = scratch_host_root.display().to_string();
    let mut env: Vec<(String, String)> = host_env
        .iter()
        .map(|(name, value)| {
            let rewritten = if value == &root {
                SCRATCH_DEST.to_string()
            } else if let Some(rest) = value.strip_prefix(&format!("{root}/")) {
                format!("{SCRATCH_DEST}/{rest}")
            } else {
                value.clone()
            };
            (name.clone(), rewritten)
        })
        .collect();
    env.push((
        "SILO_PROXY_RELAY".into(),
        format!("{PROXY_SOCKET_DEST}:{IN_SANDBOX_PROXY_PORT}"),
    ));
    env.push(("PATH".into(), SANDBOX_PATH.into()));
    env
}

/// Inputs for one OCI runtime configuration. All host paths must be
/// absolute and canonical.
#[derive(Clone, Debug, Default)]
pub struct GvisorSpec {
    /// Host path of the workspace; mounted read/write at
    /// [`WORKSPACE_DEST`].
    pub workspace: PathBuf,
    /// Host path of the scratch space; mounted read/write at
    /// [`SCRATCH_DEST`] and at its own host path (so host-path `HOME` and
    /// `TMPDIR` values set per tool execution stay valid inside).
    pub scratch: PathBuf,
    /// Read-only host directories from the user-configured allowlist,
    /// mounted at their own paths.
    pub read_allowlist: Vec<PathBuf>,
    /// Read-only OS directories, mounted at their own paths.
    pub os_dirs: Vec<PathBuf>,
    /// Environment for the helper process, already in the in-sandbox view
    /// (see [`container_env`]).
    pub env: Vec<(String, String)>,
}

fn ro_bind(source: &Path, destination: &str) -> Value {
    json!({
        "destination": destination,
        "type": "bind",
        "source": source.display().to_string(),
        "options": ["bind", "ro", "nosuid", "nodev"],
    })
}

fn rw_bind(source: &Path, destination: &str) -> Value {
    json!({
        "destination": destination,
        "type": "bind",
        "source": source.display().to_string(),
        "options": ["bind", "rw", "nosuid", "nodev"],
    })
}

/// Builds the OCI `config.json` for `spec`. The rootfs is an empty
/// read-only directory named `rootfs` in the bundle; everything visible
/// inside the sandbox comes from the mounts.
pub fn generate_config(spec: &GvisorSpec) -> Value {
    let mut mounts = vec![
        json!({
            "destination": "/proc",
            "type": "proc",
            "source": "proc",
        }),
        json!({
            "destination": "/dev",
            "type": "tmpfs",
            "source": "tmpfs",
            "options": ["nosuid", "strictatime", "mode=755", "size=65536k"],
        }),
        json!({
            "destination": "/dev/pts",
            "type": "devpts",
            "source": "devpts",
            "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"],
        }),
        json!({
            "destination": "/dev/shm",
            "type": "tmpfs",
            "source": "shm",
            "options": ["nosuid", "noexec", "nodev", "mode=1777", "size=65536k"],
        }),
        json!({
            "destination": "/tmp",
            "type": "tmpfs",
            "source": "tmpfs",
            "options": ["nosuid", "nodev", "mode=1777"],
        }),
    ];
    for dir in &spec.os_dirs {
        mounts.push(ro_bind(dir, &dir.display().to_string()));
    }
    for dir in &spec.read_allowlist {
        mounts.push(ro_bind(dir, &dir.display().to_string()));
    }
    mounts.push(rw_bind(&spec.workspace, WORKSPACE_DEST));
    mounts.push(rw_bind(&spec.scratch, SCRATCH_DEST));
    // Second scratch view at the host path; see the field documentation.
    mounts.push(rw_bind(&spec.scratch, &spec.scratch.display().to_string()));

    let env: Vec<String> = spec
        .env
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();

    json!({
        "ociVersion": "1.1.0",
        "process": {
            "terminal": false,
            "user": {"uid": 0, "gid": 0},
            "args": [HELPER_DEST, HELPER_CONNECT],
            "env": env,
            "cwd": WORKSPACE_DEST,
            "capabilities": {
                "bounding": [],
                "effective": [],
                "inheritable": [],
                "permitted": [],
                "ambient": [],
            },
            "noNewPrivileges": true,
        },
        "root": {"path": "rootfs", "readonly": true},
        "hostname": "silo-sandbox",
        "mounts": mounts,
        "linux": {
            "namespaces": [
                {"type": "pid"},
                {"type": "ipc"},
                {"type": "uts"},
                {"type": "mount"},
                {"type": "network"},
            ],
            "maskedPaths": MASKED_PATHS,
            "readonlyPaths": READONLY_PATHS,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> GvisorSpec {
        GvisorSpec {
            workspace: PathBuf::from("/home/user/ws-mount"),
            scratch: PathBuf::from("/tmp/silo-scratch-abc"),
            read_allowlist: vec![PathBuf::from("/opt/tools")],
            os_dirs: vec![PathBuf::from("/usr"), PathBuf::from("/bin")],
            env: vec![("HOME".into(), "/scratch/home".into())],
        }
    }

    fn mounts(config: &Value) -> Vec<(String, String, Vec<String>)> {
        config["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["destination"].as_str().unwrap().to_string(),
                    m["source"].as_str().unwrap().to_string(),
                    m["options"]
                        .as_array()
                        .map(|opts| {
                            opts.iter()
                                .map(|o| o.as_str().unwrap().to_string())
                                .collect()
                        })
                        .unwrap_or_default(),
                )
            })
            .collect()
    }

    #[test]
    fn process_runs_the_helper_with_no_capabilities() {
        let config = generate_config(&spec());
        assert_eq!(
            config["process"]["args"],
            json!([HELPER_DEST, HELPER_CONNECT])
        );
        assert_eq!(config["process"]["cwd"], WORKSPACE_DEST);
        assert_eq!(config["process"]["noNewPrivileges"], true);
        for set in [
            "bounding",
            "effective",
            "inheritable",
            "permitted",
            "ambient",
        ] {
            assert_eq!(
                config["process"]["capabilities"][set],
                json!([]),
                "capability set {set} is not empty"
            );
        }
        assert_eq!(config["root"]["readonly"], true);
    }

    #[test]
    fn mounts_cover_os_dirs_allowlist_workspace_and_scratch() {
        let config = generate_config(&spec());
        let mounts = mounts(&config);
        let find = |dest: &str| {
            mounts
                .iter()
                .find(|(d, _, _)| d == dest)
                .unwrap_or_else(|| panic!("missing mount {dest}"))
        };

        for ro in ["/usr", "/bin", "/opt/tools"] {
            let (_, source, options) = find(ro);
            assert_eq!(source, ro);
            assert!(options.contains(&"ro".to_string()), "{ro} is not read-only");
        }
        let (_, source, options) = find(WORKSPACE_DEST);
        assert_eq!(source, "/home/user/ws-mount");
        assert!(options.contains(&"rw".to_string()));
        let (_, source, options) = find(SCRATCH_DEST);
        assert_eq!(source, "/tmp/silo-scratch-abc");
        assert!(options.contains(&"rw".to_string()));
        // The scratch is also visible at its host path.
        let (_, source, _) = find("/tmp/silo-scratch-abc");
        assert_eq!(source, "/tmp/silo-scratch-abc");
        for special in ["/proc", "/dev", "/tmp"] {
            find(special);
        }
    }

    #[test]
    fn allowlist_mounts_are_never_writable() {
        let config = generate_config(&spec());
        for (dest, _, options) in mounts(&config) {
            if dest == WORKSPACE_DEST
                || dest.starts_with(SCRATCH_DEST)
                || dest == "/tmp/silo-scratch-abc"
            {
                continue;
            }
            assert!(
                !options.contains(&"rw".to_string()),
                "mount {dest} is writable"
            );
        }
    }

    #[test]
    fn linux_section_has_network_namespace_and_hardening() {
        let config = generate_config(&spec());
        let namespaces: Vec<&str> = config["linux"]["namespaces"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap())
            .collect();
        for ns in ["pid", "ipc", "uts", "mount", "network"] {
            assert!(namespaces.contains(&ns), "missing {ns} namespace");
        }
        assert!(config["linux"]["maskedPaths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "/proc/kcore"));
        assert!(config["linux"]["readonlyPaths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "/proc/sys"));
    }

    #[test]
    fn container_env_rewrites_scratch_paths_and_adds_relay() {
        let host_env = vec![
            ("HOME".to_string(), "/tmp/silo-scratch-abc/home".to_string()),
            (
                "TMPDIR".to_string(),
                "/tmp/silo-scratch-abc/tmp".to_string(),
            ),
            (
                "SILO_PROXY_CA".to_string(),
                "/tmp/silo-scratch-abc/proxy-ca.pem".to_string(),
            ),
            (
                "HTTP_PROXY".to_string(),
                "http://127.0.0.1:3128".to_string(),
            ),
            ("X_ROOT".to_string(), "/tmp/silo-scratch-abc".to_string()),
            (
                "X_LOOKALIKE".to_string(),
                "/tmp/silo-scratch-abcdef".to_string(),
            ),
        ];
        let env = container_env(&host_env, Path::new("/tmp/silo-scratch-abc"));
        let get = |name: &str| {
            env.iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("missing env var {name}"))
        };
        assert_eq!(get("HOME"), "/scratch/home");
        assert_eq!(get("TMPDIR"), "/scratch/tmp");
        assert_eq!(get("SILO_PROXY_CA"), "/scratch/proxy-ca.pem");
        assert_eq!(get("HTTP_PROXY"), "http://127.0.0.1:3128");
        assert_eq!(get("X_ROOT"), "/scratch");
        // A path that merely shares the prefix string is not rewritten.
        assert_eq!(get("X_LOOKALIKE"), "/tmp/silo-scratch-abcdef");
        assert_eq!(get("SILO_PROXY_RELAY"), "/scratch/proxy.sock:3128");
        assert_eq!(get("PATH"), SANDBOX_PATH);
    }

    #[test]
    fn default_os_dirs_filters_by_existence() {
        let dirs = default_os_dirs(|path| path == Path::new("/usr") || path == Path::new("/bin"));
        assert_eq!(dirs, [PathBuf::from("/usr"), PathBuf::from("/bin")]);
        assert!(default_os_dirs(|_| false).is_empty());
    }

    #[test]
    fn env_serializes_as_name_equals_value() {
        let config = generate_config(&spec());
        let env: Vec<&str> = config["process"]["env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e.as_str().unwrap())
            .collect();
        assert!(env.contains(&"HOME=/scratch/home"));
    }
}
