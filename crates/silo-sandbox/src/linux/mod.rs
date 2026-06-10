//! Linux sandbox backends: gVisor (runsc) and Firecracker-style microVMs.
//! See `docs/sandbox-backends.md` for the trust model and design of each
//! backend.

pub mod forward;
pub mod gvisor;
pub mod microvm;
pub mod spec;
