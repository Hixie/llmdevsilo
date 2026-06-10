//! Per-sandbox scratch space.
//!
//! Every sandbox gets one private writable directory outside the
//! workspace. It holds the sandboxed `HOME`, a temporary directory, and
//! the session CA public certificate (`proxy-ca.pem`). The directory is
//! created with mode 0700 and removed on shutdown (and on drop).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use silo_core::error::SandboxError;

/// Environment variable names that point TLS clients at a CA bundle.
const CA_ENV_VARS: &[&str] = &[
    "SILO_PROXY_CA",
    "SSL_CERT_FILE",
    "CURL_CA_BUNDLE",
    "GIT_SSL_CAINFO",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "CARGO_HTTP_CAINFO",
];

/// A private writable directory for one sandbox: `home/`, `tmp/`, and the
/// session CA certificate as `proxy-ca.pem`.
pub struct ScratchSpace {
    root: PathBuf,
    cleaned: AtomicBool,
}

impl ScratchSpace {
    /// Creates `silo-scratch-<random>` under `scratch_root` (or the
    /// platform temporary directory), mode 0700, with `home/`, `tmp/`, and
    /// `proxy-ca.pem` holding `ca_cert_pem`.
    pub fn create(
        scratch_root: Option<&Path>,
        ca_cert_pem: &str,
    ) -> Result<ScratchSpace, SandboxError> {
        let parent = scratch_root
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir);
        std::fs::create_dir_all(&parent)?;
        let mut root = None;
        for _ in 0..8 {
            let candidate = parent.join(format!("silo-scratch-{}", silo_core::short_id()));
            match std::fs::create_dir(&candidate) {
                Ok(()) => {
                    root = Some(candidate);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        let root = root.ok_or_else(|| {
            SandboxError::Setup("cannot create a unique scratch directory".into())
        })?;
        // From here on, dropping `space` removes the partially built tree.
        let space = ScratchSpace {
            root,
            cleaned: AtomicBool::new(false),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&space.root, std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::create_dir(space.home_dir())?;
        std::fs::create_dir(space.tmp_dir())?;
        std::fs::write(space.ca_cert_path(), ca_cert_pem)?;
        Ok(space)
    }

    /// The scratch directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The sandboxed `HOME`.
    pub fn home_dir(&self) -> PathBuf {
        self.root.join("home")
    }

    /// The sandboxed `TMPDIR`.
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// The session CA public certificate.
    pub fn ca_cert_path(&self) -> PathBuf {
        self.root.join("proxy-ca.pem")
    }

    /// The environment for processes inside the sandbox (the helper and
    /// every `Bash` execution): proxy variables pointing at
    /// `proxy_http_addr`, CA-bundle variables pointing at `proxy-ca.pem`,
    /// and `HOME`/`TMPDIR` inside the scratch space.
    pub fn sandbox_env(&self, proxy_http_addr: SocketAddr) -> Vec<(String, String)> {
        let proxy = format!("http://{proxy_http_addr}");
        let ca = self.ca_cert_path().display().to_string();
        let mut env: Vec<(String, String)> = vec![
            ("HTTP_PROXY".into(), proxy.clone()),
            ("HTTPS_PROXY".into(), proxy.clone()),
            ("ALL_PROXY".into(), proxy),
        ];
        for name in CA_ENV_VARS {
            env.push(((*name).into(), ca.clone()));
        }
        env.push(("HOME".into(), self.home_dir().display().to_string()));
        env.push(("TMPDIR".into(), self.tmp_dir().display().to_string()));
        env
    }

    /// Removes the scratch tree. Idempotent; also attempted on drop.
    pub fn cleanup(&self) -> Result<(), SandboxError> {
        if self.cleaned.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                self.cleaned.store(false, Ordering::SeqCst);
                Err(e.into())
            }
        }
    }
}

impl Drop for ScratchSpace {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CA_PEM: &str = "-----BEGIN CERTIFICATE-----\nFAKE\n-----END CERTIFICATE-----\n";

    #[test]
    fn create_builds_the_layout() {
        let parent = tempfile::tempdir().unwrap();
        let scratch = ScratchSpace::create(Some(parent.path()), CA_PEM).unwrap();

        let name = scratch
            .root()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with("silo-scratch-"), "unexpected name: {name}");
        assert_eq!(scratch.root().parent().unwrap(), parent.path());
        assert!(scratch.home_dir().is_dir());
        assert!(scratch.tmp_dir().is_dir());
        assert_eq!(
            std::fs::read_to_string(scratch.ca_cert_path()).unwrap(),
            CA_PEM
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(scratch.root())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[test]
    fn sandbox_env_covers_proxy_ca_home_and_tmp() {
        let parent = tempfile::tempdir().unwrap();
        let scratch = ScratchSpace::create(Some(parent.path()), CA_PEM).unwrap();
        let addr: SocketAddr = "127.0.0.1:3128".parse().unwrap();
        let env = scratch.sandbox_env(addr);
        let get = |name: &str| -> &str {
            env.iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("missing env var {name}"))
        };

        for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
            assert_eq!(get(name), "http://127.0.0.1:3128");
        }
        let ca = scratch.ca_cert_path().display().to_string();
        for name in CA_ENV_VARS {
            assert_eq!(get(name), ca);
        }
        assert_eq!(get("HOME"), scratch.home_dir().display().to_string());
        assert_eq!(get("TMPDIR"), scratch.tmp_dir().display().to_string());
    }

    #[test]
    fn cleanup_removes_the_tree_and_is_idempotent() {
        let parent = tempfile::tempdir().unwrap();
        let scratch = ScratchSpace::create(Some(parent.path()), CA_PEM).unwrap();
        let root = scratch.root().to_path_buf();
        std::fs::write(root.join("home/leftover.txt"), "data").unwrap();

        scratch.cleanup().unwrap();
        assert!(!root.exists());
        scratch.cleanup().unwrap();
    }

    #[test]
    fn drop_removes_the_tree() {
        let parent = tempfile::tempdir().unwrap();
        let root = {
            let scratch = ScratchSpace::create(Some(parent.path()), CA_PEM).unwrap();
            scratch.root().to_path_buf()
        };
        assert!(!root.exists());
    }

    #[test]
    fn default_root_is_the_temp_dir() {
        let scratch = ScratchSpace::create(None, CA_PEM).unwrap();
        assert!(scratch.root().starts_with(std::env::temp_dir()));
        scratch.cleanup().unwrap();
    }
}
