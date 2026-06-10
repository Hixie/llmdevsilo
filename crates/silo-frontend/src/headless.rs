//! Headless frontend: runs a single command-line prompt to completion with
//! no user interaction.
//!
//! The first `next_user_input` call returns the configured prompt with an
//! instruction to call the Exit tool when done; every later call returns a
//! canned reminder. The Exit tool requests harness shutdown and carries the
//! final report message. Shutdown prints the final message, then one cost
//! line per backend from the latest observed cost_report events.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc};

use silo_core::config::FrontendConfig;
use silo_core::conversation::AgentId;
use silo_core::cost::UsageSnapshot;
use silo_core::error::FrontendError;
use silo_core::event::{Event, EventPayload};
use silo_core::tool::{ToolCall, ToolDef, ToolOutput};
use silo_core::traits::{Frontend, FrontendCommand, FrontendContext};

use crate::tools;

const EXIT_INSTRUCTION: &str =
    "When the task is complete, use the Exit tool with a final report message.";
const NON_INTERACTIVE_REMINDER: &str =
    "This is a non-interactive session; complete the task then use the Exit tool";

pub fn create(config: &FrontendConfig) -> Result<Box<dyn Frontend>, FrontendError> {
    let prompt = config
        .headless_prompt
        .clone()
        .ok_or_else(|| FrontendError::Setup("the headless frontend requires a prompt".into()))?;
    Ok(Box::new(HeadlessFrontend {
        prompt,
        first_input: AtomicBool::new(true),
        commands: None,
        costs: Arc::new(Mutex::new(BTreeMap::new())),
        cost_task: None,
    }))
}

type CostMap = BTreeMap<String, UsageSnapshot>;

pub struct HeadlessFrontend {
    prompt: String,
    first_input: AtomicBool,
    commands: Option<mpsc::Sender<FrontendCommand>>,
    /// Latest usage snapshot per backend, from cost_report events.
    costs: Arc<Mutex<CostMap>>,
    cost_task: Option<tokio::task::JoinHandle<()>>,
}

/// Records the latest cost_report per backend until the bus closes.
async fn track_costs(mut events: broadcast::Receiver<Event>, costs: Arc<Mutex<CostMap>>) {
    loop {
        match events.recv().await {
            Ok(event) => {
                if let EventPayload::CostReport { backend, usage, .. } = event.payload {
                    costs
                        .lock()
                        .expect("cost map poisoned")
                        .insert(backend, usage);
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// One "cost[backend]: ..." line per backend with recorded usage.
fn cost_lines(costs: &CostMap) -> Vec<String> {
    costs
        .iter()
        .map(|(backend, usage)| {
            format!(
                "cost[{backend}]: {} input tokens, {} output tokens, ${:.4}",
                usage.input_tokens, usage.output_tokens, usage.usd
            )
        })
        .collect()
}

#[async_trait]
impl Frontend for HeadlessFrontend {
    fn kind(&self) -> &'static str {
        "headless"
    }

    fn tool_defs(&self) -> Vec<ToolDef> {
        vec![tools::exit_def()]
    }

    async fn start(&mut self, ctx: FrontendContext) -> Result<(), FrontendError> {
        self.commands = Some(ctx.commands.clone());
        self.cost_task = Some(tokio::spawn(track_costs(
            ctx.bus.subscribe(),
            self.costs.clone(),
        )));
        Ok(())
    }

    async fn next_user_input(&self) -> Result<String, FrontendError> {
        if self.first_input.swap(false, Ordering::SeqCst) {
            Ok(format!("{}\n\n{}", self.prompt, EXIT_INSTRUCTION))
        } else {
            Ok(NON_INTERACTIVE_REMINDER.to_string())
        }
    }

    async fn run_tool(
        &self,
        _agent: &AgentId,
        call: &ToolCall,
    ) -> Result<ToolOutput, FrontendError> {
        if call.name != "Exit" {
            return Ok(ToolOutput::error(format!(
                "unknown frontend tool: {}",
                call.name
            )));
        }
        let message = match tools::parse_exit_message(&call.input) {
            Ok(message) => message,
            Err(error) => return Ok(ToolOutput::error(error)),
        };
        let commands = self
            .commands
            .as_ref()
            .ok_or_else(|| FrontendError::Setup("the frontend has not been started".into()))?;
        commands
            .send(FrontendCommand::Shutdown {
                message: Some(message),
            })
            .await
            .map_err(|_| FrontendError::Closed("the harness command channel is closed".into()))?;
        Ok(ToolOutput::ok("exiting"))
    }

    async fn shutdown(&mut self, message: Option<String>) -> Result<(), FrontendError> {
        if let Some(task) = self.cost_task.take() {
            task.abort();
        }
        if let Some(message) = message {
            println!("{message}");
        }
        for line in cost_lines(&self.costs.lock().expect("cost map poisoned")) {
            println!("{line}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use silo_core::clock::{FakeClock, SharedClock};
    use silo_core::config::FrontendKind;
    use silo_core::event::EventBus;
    use silo_core::journal::JournalHandle;
    use silo_core::sandbox::AccessReport;

    use super::*;

    fn config(prompt: Option<&str>) -> FrontendConfig {
        FrontendConfig {
            kind: FrontendKind::Headless,
            listen_addr: None,
            headless_prompt: prompt.map(str::to_string),
            issue_pairing_code: false,
        }
    }

    fn context(commands: mpsc::Sender<FrontendCommand>) -> FrontendContext {
        let clock: SharedClock = Arc::new(FakeClock::default());
        FrontendContext {
            harness_id: "h1".into(),
            bus: EventBus::new(clock.clone(), JournalHandle::disabled(clock)),
            commands,
            access: AccessReport::default(),
            state_dir: std::env::temp_dir(),
            workspace: "/tmp/ws".into(),
        }
    }

    #[test]
    fn create_requires_a_prompt() {
        assert!(matches!(
            create(&config(None)),
            Err(FrontendError::Setup(_))
        ));
        assert!(create(&config(Some("do the thing"))).is_ok());
    }

    #[test]
    fn contributes_only_the_exit_tool() {
        let frontend = create(&config(Some("p"))).unwrap();
        let defs = frontend.tool_defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "Exit");
    }

    #[tokio::test]
    async fn input_sequencing_returns_prompt_then_reminder() {
        let frontend = create(&config(Some("write a parser"))).unwrap();
        let first = frontend.next_user_input().await.unwrap();
        assert_eq!(
            first,
            "write a parser\n\nWhen the task is complete, use the Exit tool with a final report message."
        );
        for _ in 0..3 {
            let later = frontend.next_user_input().await.unwrap();
            assert_eq!(later, NON_INTERACTIVE_REMINDER);
        }
    }

    #[tokio::test]
    async fn exit_tool_requests_shutdown_with_the_message() {
        let mut frontend = create(&config(Some("p"))).unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        frontend.start(context(tx)).await.unwrap();

        let call = ToolCall {
            id: "t1".into(),
            name: "Exit".into(),
            input: json!({"message": "all tests pass"}),
        };
        let output = frontend
            .run_tool(&"agent-0".to_string(), &call)
            .await
            .unwrap();
        assert!(!output.is_error);
        assert_eq!(output.content, "exiting");
        assert_eq!(
            rx.recv().await.unwrap(),
            FrontendCommand::Shutdown {
                message: Some("all tests pass".into())
            }
        );
        frontend
            .shutdown(Some("all tests pass".into()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn exit_without_message_is_a_tool_error_and_does_not_shut_down() {
        let mut frontend = create(&config(Some("p"))).unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        frontend.start(context(tx)).await.unwrap();

        let call = ToolCall {
            id: "t1".into(),
            name: "Exit".into(),
            input: json!({}),
        };
        let output = frontend
            .run_tool(&"agent-0".to_string(), &call)
            .await
            .unwrap();
        assert!(output.is_error);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn unknown_tool_is_a_tool_error() {
        let frontend = create(&config(Some("p"))).unwrap();
        let call = ToolCall {
            id: "t1".into(),
            name: "AskUserQuestion".into(),
            input: json!({"question": "?"}),
        };
        let output = frontend
            .run_tool(&"agent-0".to_string(), &call)
            .await
            .unwrap();
        assert!(output.is_error);
    }

    #[tokio::test]
    async fn cost_reports_are_tracked_per_backend_with_the_latest_winning() {
        use silo_core::cost::QuotaConfig;

        let mut frontend = HeadlessFrontend {
            prompt: "p".into(),
            first_input: AtomicBool::new(true),
            commands: None,
            costs: Arc::new(Mutex::new(CostMap::new())),
            cost_task: None,
        };
        let (tx, _rx) = mpsc::channel(4);
        let ctx = context(tx);
        let bus = ctx.bus.clone();
        frontend.start(ctx).await.unwrap();

        let report = |backend: &str, input: u64, output: u64, usd: f64| EventPayload::CostReport {
            backend: backend.into(),
            usage: UsageSnapshot {
                input_tokens: input,
                output_tokens: output,
                usd,
            },
            quota: QuotaConfig::default(),
        };
        bus.emit(report("anthropic:m", 10, 5, 0.5));
        bus.emit(report("anthropic:m", 30, 12, 1.25));
        bus.emit(report("local:l", 7, 2, 0.0));
        bus.emit(EventPayload::AwaitingInput);

        // The tracking task runs concurrently; yield until it has consumed
        // the reports.
        for _ in 0..1000 {
            if frontend.costs.lock().unwrap().len() == 2
                && frontend.costs.lock().unwrap()["anthropic:m"].input_tokens == 30
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        let lines = cost_lines(&frontend.costs.lock().unwrap());
        assert_eq!(
            lines,
            vec![
                "cost[anthropic:m]: 30 input tokens, 12 output tokens, $1.2500",
                "cost[local:l]: 7 input tokens, 2 output tokens, $0.0000",
            ]
        );
        frontend.shutdown(Some("done".into())).await.unwrap();
    }

    #[test]
    fn cost_lines_format_tokens_and_dollars() {
        let mut costs = CostMap::new();
        costs.insert(
            "mock".into(),
            UsageSnapshot {
                input_tokens: 100,
                output_tokens: 40,
                usd: 0.0123,
            },
        );
        let lines = cost_lines(&costs);
        assert_eq!(
            lines,
            vec!["cost[mock]: 100 input tokens, 40 output tokens, $0.0123"]
        );
        assert!(cost_lines(&CostMap::new()).is_empty());
    }

    #[tokio::test]
    async fn exit_before_start_is_an_error() {
        let frontend = create(&config(Some("p"))).unwrap();
        let call = ToolCall {
            id: "t1".into(),
            name: "Exit".into(),
            input: json!({"message": "done"}),
        };
        assert!(matches!(
            frontend.run_tool(&"agent-0".to_string(), &call).await,
            Err(FrontendError::Setup(_))
        ));
    }
}
