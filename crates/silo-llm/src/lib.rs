//! LLM backends: Anthropic Messages REST, OpenAI Responses REST, OpenAI
//! WebSocket (Realtime, text only), a managed local model server, and a
//! scripted mock for tests.
//!
//! All backends speak the provider-agnostic conversation types from
//! `silo_core::conversation` and meter usage through
//! `silo_core::cost::UsageMeter`. Cloud backends enforce quotas before
//! every request.

use std::sync::Arc;

use silo_core::config::{LlmBackendKind, LlmConfig};
use silo_core::error::LlmError;
use silo_core::replay::SharedScript;
use silo_core::traits::LlmBackend;

pub mod anthropic;
pub mod common;
pub mod local;
pub mod mock;
pub mod openai_responses;
pub mod openai_ws;

/// Creates the configured backend. `script` is required for (and only used
/// by) the mock backend.
pub async fn create_backend(
    config: &LlmConfig,
    script: Option<SharedScript>,
) -> Result<Arc<dyn LlmBackend>, LlmError> {
    match config.backend {
        LlmBackendKind::Anthropic => anthropic::create(config).await,
        LlmBackendKind::OpenaiResponses => openai_responses::create(config).await,
        LlmBackendKind::OpenaiWebsocket => openai_ws::create(config).await,
        LlmBackendKind::Local => local::create(config).await,
        LlmBackendKind::Mock => {
            let script = script.ok_or_else(|| {
                LlmError::Config("the mock backend requires a test script".into())
            })?;
            mock::create(config, script)
        }
    }
}
