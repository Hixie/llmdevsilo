//! Frontend implementations.
//!
//! - `interactive`: a TLS WebSocket server speaking
//!   `silo_core::protocol`. Requires authentication (filesystem-shared
//!   token for local clients; pairing codes plus Ed25519
//!   challenge-signatures for remote clients). Keeps any number of
//!   connected clients synchronized over the shared event stream and
//!   contributes the AskUserQuestion and SendUserFile tools.
//! - `headless`: takes the prompt from the command line, instructs the
//!   model to call Exit when done, and answers every later input request
//!   with the canned non-interactive reminder. Contributes the Exit tool.
//! - `mock`: drives the harness from a test script and verifies observed
//!   events, for deterministic end-to-end tests.
//! - `client`: the client side of the interactive protocol (certificate
//!   pinning, connection and authentication helpers, harness discovery),
//!   shared by the TUI client and by tests.

use silo_core::config::{FrontendConfig, FrontendKind};
use silo_core::error::FrontendError;
use silo_core::replay::SharedScript;
use silo_core::traits::Frontend;

pub mod client;
pub mod headless;
pub mod interactive;
pub mod mock;

mod tools;
mod util;

/// Creates the configured frontend (not yet started). `script` is required
/// for (and only used by) the mock frontend.
pub fn create_frontend(
    config: &FrontendConfig,
    script: Option<SharedScript>,
) -> Result<Box<dyn Frontend>, FrontendError> {
    match config.kind {
        FrontendKind::Interactive => interactive::create(config),
        FrontendKind::Headless => headless::create(config),
        FrontendKind::Mock => {
            let script = script.ok_or_else(|| {
                FrontendError::Setup("the mock frontend requires a test script".into())
            })?;
            mock::create(config, script)
        }
    }
}
