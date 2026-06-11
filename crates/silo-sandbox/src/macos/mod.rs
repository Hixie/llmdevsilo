//! macOS sandbox backends: sandbox-exec (Seatbelt) and a Linux guest VM
//! via Virtualization.framework. See `docs/SANDBOX-BACKENDS.md` for the
//! trust model and design of each backend.

pub mod linux_vm;
pub mod profile;
pub mod sandbox_exec;
