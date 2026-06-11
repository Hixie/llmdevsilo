//! Linux sandbox backends: gVisor (runsc) and Firecracker-style microVMs.
//! See `docs/SANDBOX-BACKENDS.md` for the trust model and design of each
//! backend.

pub mod forward;
pub mod gvisor;
pub mod microvm;
pub mod spec;
