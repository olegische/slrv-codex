use codex_core::CodexThread;
use codex_core::ContentItem;
use codex_core::REVIEW_PROMPT;
use codex_core::ResponseItem;
use codex_core::config::Config;
use codex_core::protocol::ENVIRONMENT_CONTEXT_OPEN_TAG;
use codex_core::protocol::EventMsg;
use codex_core::protocol::ExitedReviewModeEvent;
use codex_core::protocol::Op;
use codex_core::protocol::ReviewCodeLocation;
use codex_core::protocol::ReviewFinding;
use codex_core::protocol::ReviewLineRange;
use codex_core::protocol::ReviewOutputEvent;
use codex_core::protocol::ReviewRequest;
use codex_core::protocol::ReviewTarget;
use codex_core::protocol::RolloutItem;
use codex_core::protocol::RolloutLine;
use codex_core::review_format::render_review_output_text;
use codex_protocol::user_input::UserInput;
use core_test_support::load_sse_fixture_with_id_from_str;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;
use wiremock::MockServer;

const SLRV_HEADER: &str = "# SLRV Framework (Agent-Level Declaration)";

fn require_some<T>(value: Option<T>, message: &str) -> T {
    match value {
        Some(value) => value,
        None => panic!("{message}"),
    }
}

fn require_ok<T, E: std::fmt::Display>(value: Result<T, E>, message: &str) -> T {
    match value {
        Ok(value) => value,
        Err(err) => panic!("{message}: {err}"),
    }
}

// NOTE: These assertions validate upstream Codex base instructions. When SLRV is enabled,
// base instructions are intentionally extended with responsibility and refusal constraints.
// Therefore, exact snapshot equality is only valid when slrv_enabled = false.
fn assert_instructions_match(instructions: &str, expected: &str, slrv_enabled: bool) {
    let normalized_instructions = instructions.replace("\r\n", "\n");
    let normalized_expected = expected.replace("\r\n", "\n");
    if slrv_enabled {
        assert!(
            normalized_instructions.starts_with(&normalized_expected),
            "expected instructions to start with base instructions; got: {normalized_instructions}"
        );
        assert!(
            normalized_instructions.contains(SLRV_HEADER),
            "expected SLRV header to be present; got: {normalized_instructions}"
        );
    } else {
        assert_eq!(normalized_instructions, normalized_expected);
    }
}

/// Verify that submitting `Op::Review` spawns a child task and emits
/// EnteredReviewMode -> ExitedReviewMode(None) -> TurnComplete
/// in that order when the model returns a structured review JSON payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_op_emits_lifecycle_and_review_output() {
    // Skip under Codex sandbox network restrictions.
    skip_if_no_network!();

    // Start mock Responses API server. Return a single assistant message whose
    // text is a JSON-encoded ReviewOutputEvent.
    let review_json = serde_json::json!({
        "findings": [
            {
                "title": "Prefer Stylize helpers",
                "body": "Use .dim()/.bold() chaining instead of manual Style where possible.",
                "confidence_score": 0.9,
                "priority": 1,
                "code_location": {
                    "absolute_file_path": "/tmp/file.rs",
                    "line_range": {"start": 10, "end": 20}
                }
            }
        ],
        "overall_correctness": "good",
        "overall_explanation": "All good with some improvements suggested.",
        "overall_confidence_score": 0.8
    })
    .to_string();
    let sse_template = r#"[
            {"type":"response.output_item.done", "item":{
                "type":"message", "role":"assistant",
                "content":[{"type":"output_text","text":__REVIEW__}]
            }},
            {"type":"response.completed", "response": {"id": "__ID__"}}
        ]"#;
    let review_json_escaped =
        require_ok(serde_json::to_string(&review_json), "serialize review json");
    let sse_raw = sse_template.replace("__REVIEW__", &review_json_escaped);
    let (server, _request_log) = start_responses_server_with_sse(&sse_raw, 1).await;
    let codex_home = Arc::new(require_ok(TempDir::new(), "create temp dir"));
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    // Submit review request.
    require_ok(
        codex
            .submit(Op::Review {
                review_request: ReviewRequest {
                    target: ReviewTarget::Custom {
                        instructions: "Please review my changes".to_string(),
                    },
                    user_facing_hint: None,
                },
            })
            .await,
        "submit review",
    );

    // Verify lifecycle: Entered -> Exited(Some(review)) -> TurnComplete.
    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let closed = wait_for_event(&codex, |ev| matches!(ev, EventMsg::ExitedReviewMode(_))).await;
    let review = match closed {
        EventMsg::ExitedReviewMode(ev) => require_some(
            ev.review_output,
            "expected ExitedReviewMode with Some(review_output)",
        ),
        other => panic!("expected ExitedReviewMode(..), got {other:?}"),
    };

    // Deep compare full structure using PartialEq (floats are f32 on both sides).
    let expected = ReviewOutputEvent {
        findings: vec![ReviewFinding {
            title: "Prefer Stylize helpers".to_string(),
            body: "Use .dim()/.bold() chaining instead of manual Style where possible.".to_string(),
            confidence_score: 0.9,
            priority: 1,
            code_location: ReviewCodeLocation {
                absolute_file_path: PathBuf::from("/tmp/file.rs"),
                line_range: ReviewLineRange { start: 10, end: 20 },
            },
        }],
        overall_correctness: "good".to_string(),
        overall_explanation: "All good with some improvements suggested.".to_string(),
        overall_confidence_score: 0.8,
    };
    assert_eq!(expected, review);
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    // Also verify that a user message with the header and a formatted finding
    // was recorded back in the parent session's rollout.
    let path = require_some(codex.rollout_path(), "rollout path");
    let text = require_ok(std::fs::read_to_string(&path), "read rollout file");

    let mut saw_header = false;
    let mut saw_finding_line = false;
    let expected_assistant_text = render_review_output_text(&expected);
    let mut saw_assistant_plain = false;
    let mut saw_assistant_xml = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = require_ok(serde_json::from_str(line), "jsonl line");
        let rl: RolloutLine = require_ok(serde_json::from_value(v), "rollout line");
        if let RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) = rl.item {
            if role == "user" {
                for c in content {
                    if let ContentItem::InputText { text } = c {
                        if text.contains("full review output from reviewer model") {
                            saw_header = true;
                        }
                        if text.contains("- Prefer Stylize helpers — /tmp/file.rs:10-20") {
                            saw_finding_line = true;
                        }
                    }
                }
            } else if role == "assistant" {
                for c in content {
                    if let ContentItem::OutputText { text } = c {
                        if text.contains("<user_action>") {
                            saw_assistant_xml = true;
                        }
                        if text == expected_assistant_text {
                            saw_assistant_plain = true;
                        }
                    }
                }
            }
        }
    }
    assert!(saw_header, "user header missing from rollout");
    assert!(
        saw_finding_line,
        "formatted finding line missing from rollout"
    );
    assert!(
        saw_assistant_plain,
        "assistant review output missing from rollout"
    );
    assert!(
        !saw_assistant_xml,
        "assistant review output contains user_action markup"
    );

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// When the model returns plain text that is not JSON, ensure the child
/// lifecycle still occurs and the plain text is surfaced via
/// ExitedReviewMode(Some(..)) as the overall_explanation.
// Windows CI only: bump to 4 workers to prevent SSE/event starvation and test timeouts.
#[cfg_attr(windows, tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[cfg_attr(not(windows), tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn review_op_with_plain_text_emits_review_fallback() {
    skip_if_no_network!();

    let sse_raw = r#"[
        {"type":"response.output_item.done", "item":{
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"just plain text"}]
        }},
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (server, _request_log) = start_responses_server_with_sse(sse_raw, 1).await;
    let codex_home = Arc::new(require_ok(TempDir::new(), "create temp dir"));
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    require_ok(
        codex
            .submit(Op::Review {
                review_request: ReviewRequest {
                    target: ReviewTarget::Custom {
                        instructions: "Plain text review".to_string(),
                    },
                    user_facing_hint: None,
                },
            })
            .await,
        "submit review",
    );

    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let closed = wait_for_event(&codex, |ev| matches!(ev, EventMsg::ExitedReviewMode(_))).await;
    let review = match closed {
        EventMsg::ExitedReviewMode(ev) => require_some(
            ev.review_output,
            "expected ExitedReviewMode with Some(review_output)",
        ),
        other => panic!("expected ExitedReviewMode(..), got {other:?}"),
    };

    // Expect a structured fallback carrying the plain text.
    let expected = ReviewOutputEvent {
        overall_explanation: "just plain text".to_string(),
        ..Default::default()
    };
    assert_eq!(expected, review);
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// Ensure review flow suppresses assistant-specific streaming/completion events:
/// - AgentMessageContentDelta
/// - AgentMessageDelta (legacy)
/// - ItemCompleted for TurnItem::AgentMessage
// Windows CI only: bump to 4 workers to prevent SSE/event starvation and test timeouts.
#[cfg_attr(windows, tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[cfg_attr(not(windows), tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn review_filters_agent_message_related_events() {
    skip_if_no_network!();

    // Stream simulating a typing assistant message with deltas and finalization.
    let sse_raw = r#"[
        {"type":"response.output_item.added", "item":{
            "type":"message", "role":"assistant", "id":"msg-1",
            "content":[{"type":"output_text","text":""}]
        }},
        {"type":"response.output_text.delta", "delta":"Hi"},
        {"type":"response.output_text.delta", "delta":" there"},
        {"type":"response.output_item.done", "item":{
            "type":"message", "role":"assistant", "id":"msg-1",
            "content":[{"type":"output_text","text":"Hi there"}]
        }},
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (server, _request_log) = start_responses_server_with_sse(sse_raw, 1).await;
    let codex_home = Arc::new(require_ok(TempDir::new(), "create temp dir"));
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    require_ok(
        codex
            .submit(Op::Review {
                review_request: ReviewRequest {
                    target: ReviewTarget::Custom {
                        instructions: "Filter streaming events".to_string(),
                    },
                    user_facing_hint: None,
                },
            })
            .await,
        "submit review",
    );

    let mut saw_entered = false;
    let mut saw_exited = false;

    // Drain until TurnComplete; assert streaming-related events never surface.
    wait_for_event(&codex, |event| match event {
        EventMsg::TurnComplete(_) => true,
        EventMsg::EnteredReviewMode(_) => {
            saw_entered = true;
            false
        }
        EventMsg::ExitedReviewMode(_) => {
            saw_exited = true;
            false
        }
        // The following must be filtered by review flow
        EventMsg::AgentMessageContentDelta(_) => {
            panic!("unexpected AgentMessageContentDelta surfaced during review")
        }
        EventMsg::AgentMessageDelta(_) => {
            panic!("unexpected AgentMessageDelta surfaced during review")
        }
        _ => false,
    })
    .await;
    assert!(saw_entered && saw_exited, "missing review lifecycle events");

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// When the model returns structured JSON in a review, ensure only a single
/// non-streaming AgentMessage is emitted; the UI consumes the structured
/// result via ExitedReviewMode plus a final assistant message.
// Windows CI only: bump to 4 workers to prevent SSE/event starvation and test timeouts.
#[cfg_attr(windows, tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[cfg_attr(not(windows), tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn review_does_not_emit_agent_message_on_structured_output() {
    skip_if_no_network!();

    let review_json = serde_json::json!({
        "findings": [
            {
                "title": "Example",
                "body": "Structured review output.",
                "confidence_score": 0.5,
                "priority": 1,
                "code_location": {
                    "absolute_file_path": "/tmp/file.rs",
                    "line_range": {"start": 1, "end": 2}
                }
            }
        ],
        "overall_correctness": "ok",
        "overall_explanation": "ok",
        "overall_confidence_score": 0.5
    })
    .to_string();
    let sse_template = r#"[
            {"type":"response.output_item.done", "item":{
                "type":"message", "role":"assistant",
                "content":[{"type":"output_text","text":__REVIEW__}]
            }},
            {"type":"response.completed", "response": {"id": "__ID__"}}
        ]"#;
    let review_json_escaped =
        require_ok(serde_json::to_string(&review_json), "serialize review json");
    let sse_raw = sse_template.replace("__REVIEW__", &review_json_escaped);
    let (server, _request_log) = start_responses_server_with_sse(&sse_raw, 1).await;
    let codex_home = Arc::new(require_ok(TempDir::new(), "create temp dir"));
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    require_ok(
        codex
            .submit(Op::Review {
                review_request: ReviewRequest {
                    target: ReviewTarget::Custom {
                        instructions: "check structured".to_string(),
                    },
                    user_facing_hint: None,
                },
            })
            .await,
        "submit review",
    );

    // Drain events until TurnComplete; ensure we only see a final
    // AgentMessage (no streaming assistant messages).
    let mut saw_entered = false;
    let mut saw_exited = false;
    let mut agent_messages = 0;
    wait_for_event(&codex, |event| match event {
        EventMsg::TurnComplete(_) => true,
        EventMsg::AgentMessage(_) => {
            agent_messages += 1;
            false
        }
        EventMsg::EnteredReviewMode(_) => {
            saw_entered = true;
            false
        }
        EventMsg::ExitedReviewMode(_) => {
            saw_exited = true;
            false
        }
        _ => false,
    })
    .await;
    assert_eq!(1, agent_messages, "expected exactly one AgentMessage event");
    assert!(saw_entered && saw_exited, "missing review lifecycle events");

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// Ensure that when a custom `review_model` is set in the config, the review
/// request uses that model (and not the main chat model).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_uses_custom_review_model_from_config() {
    skip_if_no_network!();

    // Minimal stream: just a completed event
    let sse_raw = r#"[
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (server, request_log) = start_responses_server_with_sse(sse_raw, 1).await;
    let codex_home = Arc::new(require_ok(TempDir::new(), "create temp dir"));
    // Choose a review model different from the main model; ensure it is used.
    let codex = new_conversation_for_server(&server, codex_home.clone(), |cfg| {
        cfg.model = Some("gpt-4.1".to_string());
        cfg.review_model = Some("gpt-5.1".to_string());
    })
    .await;

    require_ok(
        codex
            .submit(Op::Review {
                review_request: ReviewRequest {
                    target: ReviewTarget::Custom {
                        instructions: "use custom model".to_string(),
                    },
                    user_facing_hint: None,
                },
            })
            .await,
        "submit review",
    );

    // Wait for completion
    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _closed = wait_for_event(&codex, |ev| {
        matches!(
            ev,
            EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                review_output: None
            })
        )
    })
    .await;
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    // Assert the request body model equals the configured review model
    let request = request_log.single_request();
    assert_eq!(request.path(), "/v1/responses");
    let body = request.body_json();
    assert_eq!(body["model"].as_str(), Some("gpt-5.1"));

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// Ensure that when `review_model` is not set in the config, the review request
/// uses the session model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_uses_session_model_when_review_model_unset() {
    skip_if_no_network!();

    // Minimal stream: just a completed event
    let sse_raw = r#"[
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (server, request_log) = start_responses_server_with_sse(sse_raw, 1).await;
    let codex_home = Arc::new(require_ok(TempDir::new(), "create temp dir"));
    let codex = new_conversation_for_server(&server, codex_home.clone(), |cfg| {
        cfg.model = Some("gpt-4.1".to_string());
        cfg.review_model = None;
    })
    .await;

    require_ok(
        codex
            .submit(Op::Review {
                review_request: ReviewRequest {
                    target: ReviewTarget::Custom {
                        instructions: "use session model".to_string(),
                    },
                    user_facing_hint: None,
                },
            })
            .await,
        "submit review",
    );

    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _closed = wait_for_event(&codex, |ev| {
        matches!(
            ev,
            EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                review_output: None
            })
        )
    })
    .await;
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request = request_log.single_request();
    assert_eq!(request.path(), "/v1/responses");
    let body = request.body_json();
    assert_eq!(body["model"].as_str(), Some("gpt-4.1"));

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// When a review session begins, it must not prepend prior chat history from
/// the parent session. The request `input` should contain only the review
/// prompt from the user.
// Windows CI only: bump to 4 workers to prevent SSE/event starvation and test timeouts.
#[cfg_attr(windows, tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[cfg_attr(not(windows), tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn review_input_isolated_from_parent_history() {
    review_input_isolated_from_parent_history_impl(false).await;
}

// Windows CI only: bump to 4 workers to prevent SSE/event starvation and test timeouts.
#[cfg_attr(windows, tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[cfg_attr(not(windows), tokio::test(flavor = "multi_thread", worker_threads = 2))]
async fn review_input_isolated_from_parent_history_slrv_enabled() {
    review_input_isolated_from_parent_history_impl(true).await;
}

async fn review_input_isolated_from_parent_history_impl(slrv_enabled: bool) {
    skip_if_no_network!();

    // Mock server for the single review request
    let sse_raw = r#"[
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (server, request_log) = start_responses_server_with_sse(sse_raw, 1).await;

    // Seed a parent session history via resume file with both user + assistant items.
    let codex_home = Arc::new(require_ok(TempDir::new(), "create temp dir"));

    let session_file = codex_home.path().join("resume.jsonl");
    {
        let mut f = require_ok(
            tokio::fs::File::create(&session_file).await,
            "create resume file",
        );
        let convo_id = Uuid::new_v4();
        // Proper session_meta line (enveloped) with a conversation id
        let meta_line = serde_json::json!({
            "timestamp": "2024-01-01T00:00:00.000Z",
            "type": "session_meta",
            "payload": {
                "id": convo_id,
                "timestamp": "2024-01-01T00:00:00Z",
                "cwd": ".",
                "originator": "test_originator",
                "cli_version": "test_version",
                "model_provider": "test-provider"
            }
        });
        require_ok(
            f.write_all(format!("{meta_line}\n").as_bytes()).await,
            "write session meta",
        );

        // Prior user message (enveloped response_item)
        let user = codex_protocol::models::ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![codex_protocol::models::ContentItem::InputText {
                text: "parent: earlier user message".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let user_json = require_ok(serde_json::to_value(&user), "serialize user response");
        let user_line = serde_json::json!({
            "timestamp": "2024-01-01T00:00:01.000Z",
            "type": "response_item",
            "payload": user_json
        });
        require_ok(
            f.write_all(format!("{user_line}\n").as_bytes()).await,
            "write user response",
        );

        // Prior assistant message (enveloped response_item)
        let assistant = codex_protocol::models::ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![codex_protocol::models::ContentItem::OutputText {
                text: "parent: assistant reply".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let assistant_json = require_ok(
            serde_json::to_value(&assistant),
            "serialize assistant response",
        );
        let assistant_line = serde_json::json!({
            "timestamp": "2024-01-01T00:00:02.000Z",
            "type": "response_item",
            "payload": assistant_json
        });
        require_ok(
            f.write_all(format!("{assistant_line}\n").as_bytes()).await,
            "write assistant response",
        );
    }
    let codex = resume_conversation_for_server(
        &server,
        codex_home.clone(),
        session_file.clone(),
        move |config| {
            config.slrv_enabled = slrv_enabled;
        },
    )
    .await;

    // Submit review request; it must start fresh (no parent history in `input`).
    let review_prompt = "Please review only this".to_string();
    require_ok(
        codex
            .submit(Op::Review {
                review_request: ReviewRequest {
                    target: ReviewTarget::Custom {
                        instructions: review_prompt.clone(),
                    },
                    user_facing_hint: None,
                },
            })
            .await,
        "submit review",
    );

    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _closed = wait_for_event(&codex, |ev| {
        matches!(
            ev,
            EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                review_output: None
            })
        )
    })
    .await;
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    // Assert the request `input` contains the environment context followed by the user review prompt.
    let request = request_log.single_request();
    assert_eq!(request.path(), "/v1/responses");
    let body = request.body_json();
    let input = require_some(body["input"].as_array(), "input array");
    assert!(
        input.len() >= 2,
        "expected at least environment context and review prompt"
    );

    let env_text = require_some(
        input
            .iter()
            .filter_map(|msg| msg["content"][0]["text"].as_str())
            .find(|text| text.starts_with(ENVIRONMENT_CONTEXT_OPEN_TAG)),
        "env text",
    );
    assert!(
        env_text.contains("<cwd>"),
        "environment context should include cwd"
    );

    let review_text = require_some(
        input
            .iter()
            .filter_map(|msg| msg["content"][0]["text"].as_str())
            .find(|text| *text == review_prompt),
        "review prompt text",
    );
    assert_eq!(
        review_text, review_prompt,
        "user message should only contain the raw review prompt"
    );

    // Ensure the REVIEW_PROMPT rubric is sent via instructions.
    let instructions = require_some(body["instructions"].as_str(), "instructions string");
    assert_instructions_match(instructions, REVIEW_PROMPT, slrv_enabled);

    // Also verify that a user interruption note was recorded in the rollout.
    let path = require_some(codex.rollout_path(), "rollout path");
    let text = require_ok(std::fs::read_to_string(&path), "read rollout file");
    let mut saw_interruption_message = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = require_ok(serde_json::from_str(line), "jsonl line");
        let rl: RolloutLine = require_ok(serde_json::from_value(v), "rollout line");
        if let RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) = rl.item
            && role == "user"
        {
            for c in content {
                if let ContentItem::InputText { text } = c
                    && text.contains("User initiated a review task, but was interrupted.")
                {
                    saw_interruption_message = true;
                    break;
                }
            }
        }
        if saw_interruption_message {
            break;
        }
    }
    assert!(
        saw_interruption_message,
        "expected user interruption message in rollout"
    );

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// After a review thread finishes, its conversation should be visible in the
/// parent session so later turns can reference the results.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_history_surfaces_in_parent_session() {
    skip_if_no_network!();

    // Respond to both the review request and the subsequent parent request.
    let sse_raw = r#"[
        {"type":"response.output_item.done", "item":{
            "type":"message", "role":"assistant",
            "content":[{"type":"output_text","text":"review assistant output"}]
        }},
        {"type":"response.completed", "response": {"id": "__ID__"}}
    ]"#;
    let (server, request_log) = start_responses_server_with_sse(sse_raw, 2).await;
    let codex_home = Arc::new(require_ok(TempDir::new(), "create temp dir"));
    let codex = new_conversation_for_server(&server, codex_home.clone(), |_| {}).await;

    // 1) Run a review turn that produces an assistant message (isolated in child).
    require_ok(
        codex
            .submit(Op::Review {
                review_request: ReviewRequest {
                    target: ReviewTarget::Custom {
                        instructions: "Start a review".to_string(),
                    },
                    user_facing_hint: None,
                },
            })
            .await,
        "submit review",
    );
    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _closed = wait_for_event(&codex, |ev| {
        matches!(
            ev,
            EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                review_output: Some(_)
            })
        )
    })
    .await;
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    // 2) Continue in the parent session; request input must not include any review items.
    let followup = "back to parent".to_string();
    require_ok(
        codex
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: followup.clone(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
            })
            .await,
        "submit followup",
    );
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    // Inspect the second request (parent turn) input contents.
    // Parent turns include session initial messages (user_instructions, environment_context).
    // Critically, no messages from the review thread should appear.
    let requests = request_log.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.path(), "/v1/responses");
    }
    let body = requests[1].body_json();
    let input = require_some(body["input"].as_array(), "input array");

    // Must include the followup as the last item for this turn
    let last = require_some(input.last(), "at least one item in input");
    assert_eq!(last["role"].as_str(), Some("user"));
    assert_eq!(last["content"][0]["text"].as_str(), Some(followup.as_str()));

    // Ensure review-thread content is present for downstream turns.
    let contains_review_rollout_user = input.iter().any(|msg| {
        msg["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("User initiated a review task.")
    });
    let contains_review_assistant = input.iter().any(|msg| {
        msg["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("review assistant output")
    });
    assert!(
        contains_review_rollout_user,
        "review rollout user message missing from parent turn input"
    );
    assert!(
        contains_review_assistant,
        "review assistant output missing from parent turn input"
    );

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// `/review` should use the session's current cwd (including runtime overrides)
/// when resolving base-branch review prompts (merge-base computation).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_uses_overridden_cwd_for_base_branch_merge_base() {
    skip_if_no_network!();

    let sse_raw = r#"[{"type":"response.completed", "response": {"id": "__ID__"}}]"#;
    let (server, request_log) = start_responses_server_with_sse(sse_raw, 1).await;

    let initial_cwd = require_ok(TempDir::new(), "create initial cwd");

    let repo_dir = require_ok(TempDir::new(), "create repo dir");
    let repo_path = repo_dir.path();

    fn run_git(repo_path: &std::path::Path, args: &[&str]) {
        let output = require_ok(
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo_path)
                .args(args)
                .output(),
            "spawn git",
        );
        assert!(
            output.status.success(),
            "git {:?} failed: stdout={:?} stderr={:?}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    run_git(repo_path, &["init", "-b", "main"]);
    run_git(repo_path, &["config", "user.email", "test@example.com"]);
    run_git(repo_path, &["config", "user.name", "Test User"]);
    require_ok(
        std::fs::write(repo_path.join("file.txt"), "hello\n"),
        "write repo file",
    );
    run_git(repo_path, &["add", "."]);
    run_git(repo_path, &["commit", "-m", "initial"]);

    let head_sha = require_ok(
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(["rev-parse", "HEAD"])
            .output(),
        "rev-parse HEAD",
    );
    assert!(head_sha.status.success());
    let head_sha = require_ok(String::from_utf8(head_sha.stdout), "utf8 sha")
        .trim()
        .to_string();

    let codex_home = Arc::new(require_ok(TempDir::new(), "create temp dir"));
    let initial_cwd_path = initial_cwd.path().to_path_buf();
    let codex = new_conversation_for_server(&server, codex_home.clone(), move |config| {
        config.cwd = initial_cwd_path;
    })
    .await;

    require_ok(
        codex
            .submit(Op::OverrideTurnContext {
                cwd: Some(repo_path.to_path_buf()),
                approval_policy: None,
                sandbox_policy: None,
                windows_sandbox_level: None,
                model: None,
                effort: None,
                summary: None,
                collaboration_mode: None,
                personality: None,
            })
            .await,
        "override turn context",
    );

    require_ok(
        codex
            .submit(Op::Review {
                review_request: ReviewRequest {
                    target: ReviewTarget::BaseBranch {
                        branch: "main".to_string(),
                    },
                    user_facing_hint: None,
                },
            })
            .await,
        "submit review",
    );

    let _entered = wait_for_event(&codex, |ev| matches!(ev, EventMsg::EnteredReviewMode(_))).await;
    let _complete = wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let requests = request_log.requests();
    assert_eq!(requests.len(), 1);
    for request in &requests {
        assert_eq!(request.path(), "/v1/responses");
    }
    let body = requests[0].body_json();
    let input = require_some(body["input"].as_array(), "input array");

    let saw_merge_base_sha = input
        .iter()
        .filter_map(|msg| msg["content"][0]["text"].as_str())
        .any(|text| text.contains(&head_sha));
    assert!(
        saw_merge_base_sha,
        "expected review prompt to include merge-base sha {head_sha}"
    );

    let _codex_home_guard = codex_home;
    server.verify().await;
}

/// Start a mock Responses API server and mount the given SSE stream body.
async fn start_responses_server_with_sse(
    sse_raw: &str,
    expected_requests: usize,
) -> (MockServer, ResponseMock) {
    let server = MockServer::start().await;
    let sse = load_sse_fixture_with_id_from_str(sse_raw, &Uuid::new_v4().to_string());
    let responses = vec![sse; expected_requests];
    let request_log = mount_sse_sequence(&server, responses).await;
    (server, request_log)
}

/// Create a conversation configured to talk to the provided mock server.
#[expect(clippy::expect_used)]
async fn new_conversation_for_server<F>(
    server: &MockServer,
    codex_home: Arc<TempDir>,
    mutator: F,
) -> Arc<CodexThread>
where
    F: FnOnce(&mut Config) + Send + 'static,
{
    let base_url = format!("{}/v1", server.uri());
    let mut builder = test_codex()
        .with_home(codex_home)
        .with_config(move |config| {
            config.model_provider.base_url = Some(base_url.clone());
            mutator(config);
        });
    builder
        .build(server)
        .await
        .expect("create conversation")
        .codex
}

/// Create a conversation resuming from a rollout file, configured to talk to the provided mock server.
#[expect(clippy::expect_used)]
async fn resume_conversation_for_server<F>(
    server: &MockServer,
    codex_home: Arc<TempDir>,
    resume_path: std::path::PathBuf,
    mutator: F,
) -> Arc<CodexThread>
where
    F: FnOnce(&mut Config) + Send + 'static,
{
    let base_url = format!("{}/v1", server.uri());
    let mut builder = test_codex()
        .with_home(codex_home.clone())
        .with_config(move |config| {
            config.model_provider.base_url = Some(base_url.clone());
            mutator(config);
        });
    builder
        .resume(server, codex_home, resume_path)
        .await
        .expect("resume conversation")
        .codex
}
