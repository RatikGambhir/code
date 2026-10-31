//! End-to-end checks against a real Claude Code.
//!
//! Skipped unless `COCKPIT_CLAUDE_BIN` points at a `claude` executable:
//!
//! ```sh
//! COCKPIT_CLAUDE_BIN=$(which claude) cargo test -p cockpit-agents --test live_claude
//! ```
//!
//! This starts Claude Code and reads its version, account and model list,
//! which bills nothing. Set `COCKPIT_CLAUDE_LIVE_TURN=1` as well to send a
//! real prompt on the account Claude Code is signed in to (it uses
//! `COCKPIT_CLAUDE_LIVE_MODEL`, Claude Haiku 5.5 by default).

use std::time::Duration;

use cockpit_agents::{
    protocol::{ApprovalDecision, SandboxMode, SandboxPolicy, TurnStatus, UserInput},
    AgentEvent, AgentRuntime, ClaudeLaunch, Prompt, ThreadConfig,
};

#[test]
fn version_account_models_and_a_turn_against_claude_code() {
    let Some(bin) = std::env::var_os("COCKPIT_CLAUDE_BIN") else {
        eprintln!("COCKPIT_CLAUDE_BIN not set; skipping");
        return;
    };
    let runtime = AgentRuntime::new().unwrap();
    let workdir = tempfile::tempdir().unwrap();
    let connection =
        futures::executor::block_on(runtime.connect_claude(ClaudeLaunch::new(bin), workdir.path().to_path_buf()))
            .expect("claude --version");
    assert!(connection.server.user_agent.starts_with("Claude Code "));

    let account = futures::executor::block_on(connection.backend.account()).expect("claude auth status");
    eprintln!("signed in: {}", !account.requires_openai_auth);

    let models = futures::executor::block_on(connection.backend.models()).expect("models");
    assert_eq!(models[0].id, cockpit_agents::claude::DEFAULT_MODEL);
    eprintln!("models: {:?}", models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>());

    if std::env::var_os("COCKPIT_CLAUDE_LIVE_TURN").is_none() {
        connection.backend.shutdown();
        return;
    }

    let model = std::env::var("COCKPIT_CLAUDE_LIVE_MODEL").unwrap_or_else(|_| "claude-haiku-5-5".into());
    let thread = futures::executor::block_on(connection.backend.start_thread(ThreadConfig {
        cwd: Some(workdir.path().to_string_lossy().into_owned()),
        model: Some(model.clone()),
        sandbox: Some(SandboxMode::ReadOnly),
        ..Default::default()
    }))
    .expect("start thread");
    let turn = futures::executor::block_on(connection.backend.submit_prompt(Prompt {
        thread_id: thread.thread.id.clone(),
        input: vec![UserInput::text("Run `echo pong` with your Bash tool, then reply with the single word: pong")],
        model: Some(model),
        effort: Some("low".into()),
        approval_policy: None,
        sandbox_policy: Some(SandboxPolicy::ReadOnly),
    }))
    .expect("submit");

    // Whatever happens (success, or an auth/network failure), the turn must
    // complete and say so. Approvals are declined so nothing runs.
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    let mut completed = None;
    while std::time::Instant::now() < deadline && completed.is_none() {
        match connection.events.recv_blocking() {
            Ok(AgentEvent::ApprovalRequested(approval)) => {
                eprintln!("approval: {:?}", approval.kind);
                connection.backend.resolve_approval(&approval.request_id, ApprovalDecision::Decline).unwrap();
            }
            Ok(AgentEvent::TurnCompleted { turn: done, .. }) if done.id == turn.id => completed = Some(done),
            Ok(event) => eprintln!("event: {}", event.kind()),
            Err(_) => break,
        }
    }
    let completed = completed.expect("turn completed");
    eprintln!("turn finished: {:?} {:?}", completed.status, completed.error);
    assert_ne!(completed.status, TurnStatus::InProgress);
    connection.backend.shutdown();
}
