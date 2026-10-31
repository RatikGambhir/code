//! End-to-end checks against a real `codex app-server`.
//!
//! Skipped unless `HANGAR_CODEX_BIN` points at a `codex` executable:
//!
//! ```sh
//! HANGAR_CODEX_BIN=$(which codex) cargo test -p cockpit-agents --test live_codex
//! ```
//!
//! Set `HANGAR_CODEX_LIVE_TURN=1` as well to send a real prompt (this uses
//! the account Codex is logged into).

use std::time::Duration;

use cockpit_agents::{
    protocol::{AskForApproval, DynamicToolSpec, SandboxMode, TurnStatus, UserInput},
    AgentEvent, AgentRuntime, CodexLaunch, Prompt, ThreadConfig,
};

fn launch() -> Option<CodexLaunch> {
    let bin = std::env::var_os("HANGAR_CODEX_BIN")?;
    let mut launch = CodexLaunch::new(bin);
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        launch.env.push(("CODEX_HOME".into(), home.to_string_lossy().into_owned()));
    }
    Some(launch)
}

#[test]
fn handshake_models_and_thread_against_a_real_server() {
    let Some(launch) = launch() else {
        eprintln!("HANGAR_CODEX_BIN not set; skipping");
        return;
    };
    let runtime = AgentRuntime::new().unwrap();
    let workdir = std::env::temp_dir();
    let connection = futures::executor::block_on(runtime.connect_codex(launch, workdir.clone(), true))
        .expect("handshake");
    assert!(!connection.server.user_agent.is_empty());

    let models = futures::executor::block_on(connection.backend.models()).expect("model/list");
    assert!(!models.is_empty(), "the server offers models");
    assert!(models.iter().all(|m| !m.id.is_empty()));

    let account = futures::executor::block_on(connection.backend.account()).expect("account/read");
    eprintln!("account: {:?} (auth required: {})", account.account, account.requires_openai_auth);

    let thread = futures::executor::block_on(connection.backend.start_thread(ThreadConfig {
        cwd: Some(workdir.to_string_lossy().into_owned()),
        approval_policy: Some(AskForApproval::OnRequest),
        sandbox: Some(SandboxMode::ReadOnly),
        dynamic_tools: vec![DynamicToolSpec::Namespace {
            name: "ide_browser".into(),
            description: "Test tools".into(),
            tools: vec![DynamicToolSpec::Function {
                name: "ping".into(),
                description: "Replies pong".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            }],
        }],
        ..Default::default()
    }))
    .expect("thread/start with dynamic tools");
    assert!(!thread.thread.id.is_empty());

    if std::env::var_os("HANGAR_CODEX_LIVE_TURN").is_none() {
        connection.backend.shutdown();
        return;
    }

    let turn = futures::executor::block_on(connection.backend.submit_prompt(Prompt {
        thread_id: thread.thread.id.clone(),
        input: vec![UserInput::text("Reply with the single word: pong")],
        model: None,
        effort: None,
        approval_policy: None,
        sandbox_policy: None,
    }))
    .expect("turn/start");

    // Whatever happens (success, or an auth/network failure), the turn must
    // complete and say so.
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    let mut completed = None;
    while std::time::Instant::now() < deadline && completed.is_none() {
        match connection.events.recv_blocking() {
            Ok(AgentEvent::TurnCompleted { turn: done, .. }) if done.id == turn.id => {
                completed = Some(done)
            }
            Ok(event) => eprintln!("event: {}", event.kind()),
            Err(_) => break,
        }
    }
    let completed = completed.expect("turn/completed");
    eprintln!("turn finished: {:?} {:?}", completed.status, completed.error);
    assert_ne!(completed.status, TurnStatus::InProgress);
    connection.backend.shutdown();
}
