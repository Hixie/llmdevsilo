//! AskUserQuestion flow: the answer from the (scripted) user reaches the
//! next LLM turn as the tool result.

mod common;

use serde_json::json;

use silo_core::conversation::{StopReason, TokenDelta};
use silo_core::event::EventPayload;
use silo_core::replay::{FrontendStep, TestScript};

#[tokio::test]
async fn question_answer_reaches_the_model() {
    let fixture = common::Fixture::new();
    let script = common::shared(TestScript {
        name: "question_session".into(),
        llm: vec![
            common::llm_turn(
                Some("ask me"),
                None,
                &[(
                    "t1",
                    "AskUserQuestion",
                    json!({"question": "Which color should the theme use?"}),
                )],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
            common::llm_turn(
                Some("blue"),
                Some("Blue it is."),
                &[("t2", "Exit", json!({"message": "picked blue"}))],
                StopReason::ToolUse,
                TokenDelta::default(),
            ),
        ],
        tools: vec![],
        frontend: vec![
            FrontendStep::SendPrompt {
                text: "ask me".into(),
            },
            FrontendStep::AnswerQuestion {
                contains: Some("color".into()),
                answer: "blue".into(),
            },
            FrontendStep::ExpectShutdown {
                message_contains: Some("picked blue".into()),
            },
        ],
        network: vec![],
    });

    let outcome = silo_harness::run(fixture.config(), fixture.options(script.clone()))
        .await
        .expect("session completes");
    assert_eq!(outcome.message.as_deref(), Some("picked blue"));
    assert!(
        script.finished(),
        "remaining: {}",
        script.remaining_summary()
    );

    let events = fixture.events();
    let asked: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::QuestionAsked { id, question, .. } => {
                Some((id.clone(), question.question.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].1.contains("color"));
    let answered: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::QuestionAnswered { id, answer, .. } => Some((id.clone(), answer.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0].0, asked[0].0);
    assert_eq!(answered[0].1, "blue");
}
