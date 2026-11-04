//! `@chatgpt`: plain ChatGPT conversations on the user's ChatGPT plan.
//!
//! No Codex in between: each turn is one streamed `POST /v1/responses` with
//! Hangar's own access token ([`super::access_token`]). OpenAI's plan-usage
//! flow requires `store: false`, so the conversation is kept here, one JSON
//! file per thread, and sent back in full as `input` every turn. Progress is
//! reported as the same [`AgentEvent`]s Codex produces, so the panel's
//! transcript, cards and usage tracking work unchanged.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    backend::{AgentBackend, Prompt, ThreadConfig},
    events::AgentEvent,
    protocol::*,
    rpc::RequestId,
};

const RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const MODELS_URL: &str = "https://api.openai.com/v1/models";
const USAGE_URL: &str = "https://chatgpt.com/settings/usage";
const INSTRUCTIONS: &str = "You are ChatGPT, talking with a software developer inside Hangar, a code editor. \
Answer clearly and concisely, and use Markdown with fenced code blocks for code. You cannot see or change \
their files or run commands; when that would help, say what to do or suggest asking @codex.";

/// One saved conversation.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ChatThread {
    model: String,
    #[serde(default)]
    effort: Option<String>,
    created_at: i64,
    messages: Vec<ChatMessage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ChatMessage {
    /// "user" or "assistant".
    role: String,
    text: String,
}

#[derive(Clone)]
pub struct ChatGptClient {
    inner: Arc<Inner>,
}

struct Inner {
    events: async_channel::Sender<AgentEvent>,
    runtime: tokio::runtime::Handle,
    threads: Mutex<HashMap<String, ChatThread>>,
    /// Running turns: (thread, turn) → stop flag.
    running: Mutex<HashMap<(String, String), Arc<AtomicBool>>>,
    /// The first listed model, for threads started without one.
    default_model: Mutex<Option<String>>,
}

impl ChatGptClient {
    pub fn new(runtime: tokio::runtime::Handle) -> (Self, async_channel::Receiver<AgentEvent>) {
        let (events, rx) = async_channel::unbounded();
        let inner = Inner {
            events,
            runtime,
            threads: Mutex::new(HashMap::new()),
            running: Mutex::new(HashMap::new()),
            default_model: Mutex::new(None),
        };
        (Self { inner: Arc::new(inner) }, rx)
    }

    fn emit(&self, event: AgentEvent) {
        let _ = self.inner.events.try_send(event);
    }

    fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> BoxFuture<'static, Result<T>> {
        let task = self.inner.runtime.spawn_blocking(work);
        Box::pin(async move { task.await.map_err(|e| anyhow!("ChatGPT task failed: {e}"))? })
    }

    fn thread(&self, id: &str) -> Result<ChatThread> {
        if let Some(thread) = self.inner.threads.lock().get(id) {
            return Ok(thread.clone());
        }
        let thread: ChatThread = serde_json::from_slice(
            &std::fs::read(thread_path(id)?).context("this ChatGPT conversation is no longer saved")?,
        )
        .context("reading the saved ChatGPT conversation")?;
        self.inner.threads.lock().insert(id.to_string(), thread.clone());
        Ok(thread)
    }

    fn save(&self, id: &str, thread: ChatThread) -> Result<()> {
        let path = thread_path(id)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&thread)?)?;
        std::fs::rename(&tmp, &path)?;
        self.inner.threads.lock().insert(id.to_string(), thread);
        Ok(())
    }

    fn default_model(&self) -> Result<String> {
        if let Some(model) = self.inner.default_model.lock().clone() {
            return Ok(model);
        }
        let models = fetch_models()?;
        let model = models.first().map(|m| m.id.clone()).context("no ChatGPT models are available")?;
        *self.inner.default_model.lock() = Some(model.clone());
        Ok(model)
    }

    /// Runs one turn to the end (blocking): streams the answer as events,
    /// saves the exchange, and reports how it ended.
    fn run_turn(&self, thread_id: String, turn_id: String, text: String, model: String, effort: Option<String>, stop: Arc<AtomicBool>) {
        let started = now();
        let user_item = ThreadItem::UserMessage {
            id: format!("{turn_id}-user"),
            content: vec![UserInput::text(text.clone())],
        };
        self.emit(AgentEvent::ItemStarted { thread_id: thread_id.clone(), turn_id: turn_id.clone(), item: user_item.clone() });
        self.emit(AgentEvent::ItemCompleted { thread_id: thread_id.clone(), turn_id: turn_id.clone(), item: user_item });

        let mut stream = TurnStream::new(self.clone(), thread_id.clone(), turn_id.clone());
        let outcome = self
            .thread(&thread_id)
            .and_then(|thread| {
                let mut input: Vec<Value> = thread
                    .messages
                    .iter()
                    .map(|m| json!({"role": m.role, "content": m.text}))
                    .collect();
                input.push(json!({"role": "user", "content": text}));
                let mut body = json!({
                    "model": model,
                    "instructions": INSTRUCTIONS,
                    "input": input,
                    "store": false,
                    "stream": true,
                });
                if let Some(effort) = effort.as_deref().filter(|e| !e.is_empty()) {
                    body["reasoning"] = json!({"effort": effort, "summary": "auto"});
                } else {
                    body["reasoning"] = json!({"summary": "auto"});
                }
                stream_response(&body, &stop, &mut stream)
            })
            .unwrap_or_else(|err| Outcome::Failed(format!("{err:#}")));
        let outcome = if stop.load(Ordering::Relaxed) { Outcome::Interrupted } else { outcome };
        stream.finish();

        // Keep the exchange (even a partial answer) for the next turn.
        if let Ok(mut thread) = self.thread(&thread_id) {
            thread.messages.push(ChatMessage { role: "user".into(), text });
            if !stream.text.is_empty() {
                thread.messages.push(ChatMessage { role: "assistant".into(), text: stream.text.clone() });
            }
            thread.model = model;
            if let Err(err) = self.save(&thread_id, thread) {
                tracing::warn!("saving the ChatGPT conversation: {err:#}");
            }
        }

        if let Some(usage) = stream.usage {
            self.emit(AgentEvent::TurnUsage {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                usage,
                context_tokens: Some(usage.input_tokens + usage.output_tokens),
                model_context_window: None,
            });
        }
        let (status, error) = match outcome {
            Outcome::Completed => (TurnStatus::Completed, None),
            Outcome::Interrupted => (TurnStatus::Interrupted, None),
            Outcome::Failed(message) => {
                self.emit(AgentEvent::Error {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                    message: message.clone(),
                    will_retry: false,
                });
                (TurnStatus::Failed, Some(TurnError { message, ..TurnError::default() }))
            }
        };
        self.inner.running.lock().remove(&(thread_id.clone(), turn_id.clone()));
        let completed = now();
        self.emit(AgentEvent::TurnCompleted {
            thread_id,
            turn: Turn {
                id: turn_id,
                status,
                error,
                started_at: Some(started),
                completed_at: Some(completed),
                duration_ms: Some((completed - started) * 1000),
                ..Turn::default()
            },
        });
    }
}

impl AgentBackend for ChatGptClient {
    fn name(&self) -> &str {
        "ChatGPT"
    }

    fn start_thread(&self, config: ThreadConfig) -> BoxFuture<'static, Result<ThreadStartResponse>> {
        let client = self.clone();
        self.blocking(move || {
            super::access_token().context("sign in with ChatGPT first (Codex menu → Use Hangar's ChatGPT sign-in)")?;
            let model = match config.model.filter(|m| !m.is_empty()) {
                Some(model) => model,
                None => client.default_model()?,
            };
            let id = format!("chatgpt-{}", random_hex(12)?);
            let created_at = now();
            client.save(&id, ChatThread { model: model.clone(), effort: None, created_at, messages: Vec::new() })?;
            let thread = Thread {
                id,
                model: Some(model.clone()),
                cwd: config.cwd.clone(),
                created_at: Some(created_at),
                updated_at: Some(created_at),
                ..Thread::default()
            };
            client.emit(AgentEvent::ThreadStarted(thread.clone()));
            Ok(ThreadStartResponse { thread, model, cwd: config.cwd, reasoning_effort: None })
        })
    }

    fn resume_thread(&self, thread_id: String, config: ThreadConfig) -> BoxFuture<'static, Result<ThreadResumeResponse>> {
        let client = self.clone();
        self.blocking(move || {
            let saved = client.thread(&thread_id)?;
            Ok(ThreadResumeResponse {
                thread: Thread {
                    id: thread_id.clone(),
                    model: Some(saved.model.clone()),
                    cwd: config.cwd.clone(),
                    created_at: Some(saved.created_at),
                    turns: turns_from(&thread_id, &saved.messages),
                    ..Thread::default()
                },
                model: saved.model,
                cwd: config.cwd,
                reasoning_effort: saved.effort,
            })
        })
    }

    fn submit_prompt(&self, prompt: Prompt) -> BoxFuture<'static, Result<Turn>> {
        let client = self.clone();
        Box::pin(async move {
            let text = prompt
                .input
                .iter()
                .filter_map(|input| match input {
                    UserInput::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if text.trim().is_empty() {
                bail!("ChatGPT only takes text messages");
            }
            let saved = client.thread(&prompt.thread_id)?;
            let model = prompt.model.filter(|m| !m.is_empty()).unwrap_or(saved.model);
            let turn_id = format!("turn-{}", random_hex(8)?);
            let stop = Arc::new(AtomicBool::new(false));
            client
                .inner
                .running
                .lock()
                .insert((prompt.thread_id.clone(), turn_id.clone()), stop.clone());
            let turn = Turn { id: turn_id.clone(), status: TurnStatus::InProgress, started_at: Some(now()), ..Turn::default() };
            client.emit(AgentEvent::TurnStarted { thread_id: prompt.thread_id.clone(), turn: turn.clone() });
            let worker = client.clone();
            let (thread_id, effort) = (prompt.thread_id, prompt.effort);
            client
                .inner
                .runtime
                .spawn_blocking(move || worker.run_turn(thread_id, turn_id, text, model, effort, stop));
            Ok(turn)
        })
    }

    fn interrupt(&self, thread_id: String, turn_id: String) -> BoxFuture<'static, Result<()>> {
        if let Some(stop) = self.inner.running.lock().get(&(thread_id, turn_id)) {
            stop.store(true, Ordering::Relaxed);
        }
        Box::pin(async { Ok(()) })
    }

    fn resolve_approval(&self, _: &RequestId, _: ApprovalDecision) -> Result<()> {
        bail!("ChatGPT does not ask for approvals")
    }

    fn respond_tool_call(&self, _: &RequestId, _: DynamicToolCallResponse) -> Result<()> {
        bail!("ChatGPT does not call tools")
    }

    fn models(&self) -> BoxFuture<'static, Result<Vec<Model>>> {
        let client = self.clone();
        self.blocking(move || {
            let models = fetch_models()?;
            if let Some(first) = models.first() {
                *client.inner.default_model.lock() = Some(first.id.clone());
            }
            Ok(models)
        })
    }

    fn account(&self) -> BoxFuture<'static, Result<GetAccountResponse>> {
        self.blocking(|| {
            let account = super::signed_in_account()?;
            Ok(GetAccountResponse {
                requires_openai_auth: account.is_none(),
                account: account.map(|a| Account::Chatgpt { email: a.email, plan_type: Value::Null }),
            })
        })
    }

    fn login(&self, _: LoginParams) -> BoxFuture<'static, Result<LoginResponse>> {
        Box::pin(async { Err(anyhow!("sign in with ChatGPT from the Codex menu")) })
    }

    fn logout(&self) -> BoxFuture<'static, Result<()>> {
        self.blocking(|| super::sign_out().map(|_| ()))
    }

    fn shutdown(&self) {
        for stop in self.inner.running.lock().values() {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

enum Outcome {
    Completed,
    Interrupted,
    Failed(String),
}

/// What one turn's stream has produced so far.
struct TurnStream {
    client: ChatGptClient,
    thread_id: String,
    turn_id: String,
    text: String,
    reasoning: String,
    message_started: bool,
    reasoning_started: bool,
    usage: Option<TokenUsageBreakdown>,
}

impl TurnStream {
    fn new(client: ChatGptClient, thread_id: String, turn_id: String) -> Self {
        Self {
            client,
            thread_id,
            turn_id,
            text: String::new(),
            reasoning: String::new(),
            message_started: false,
            reasoning_started: false,
            usage: None,
        }
    }

    fn message_id(&self) -> String {
        format!("{}-answer", self.turn_id)
    }

    fn reasoning_id(&self) -> String {
        format!("{}-thinking", self.turn_id)
    }

    fn text_delta(&mut self, delta: &str) {
        if !self.message_started {
            self.message_started = true;
            self.client.emit(AgentEvent::ItemStarted {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item: ThreadItem::AgentMessage { id: self.message_id(), text: String::new() },
            });
        }
        self.text.push_str(delta);
        self.client.emit(AgentEvent::AgentMessageDelta {
            thread_id: self.thread_id.clone(),
            item_id: self.message_id(),
            delta: delta.to_string(),
        });
    }

    fn reasoning_delta(&mut self, delta: &str) {
        if !self.reasoning_started {
            self.reasoning_started = true;
            self.client.emit(AgentEvent::ItemStarted {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item: ThreadItem::Reasoning { id: self.reasoning_id(), summary: Vec::new(), content: Vec::new() },
            });
        }
        self.reasoning.push_str(delta);
        self.client.emit(AgentEvent::ReasoningDelta {
            thread_id: self.thread_id.clone(),
            item_id: self.reasoning_id(),
            delta: delta.to_string(),
        });
    }

    /// Closes the items that were opened.
    fn finish(&mut self) {
        if self.reasoning_started {
            self.client.emit(AgentEvent::ItemCompleted {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item: ThreadItem::Reasoning {
                    id: self.reasoning_id(),
                    summary: vec![self.reasoning.clone()],
                    content: Vec::new(),
                },
            });
        }
        if self.message_started {
            self.client.emit(AgentEvent::ItemCompleted {
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                item: ThreadItem::AgentMessage { id: self.message_id(), text: self.text.clone() },
            });
        }
    }

    /// Applies one server-sent event; `Some` once the response has ended.
    fn apply(&mut self, event: &Value) -> Option<Outcome> {
        match event["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => self.text_delta(event["delta"].as_str().unwrap_or_default()),
            "response.reasoning_summary_text.delta" => {
                self.reasoning_delta(event["delta"].as_str().unwrap_or_default())
            }
            "response.reasoning_summary_part.added" if !self.reasoning.is_empty() => self.reasoning_delta("\n\n"),
            "response.completed" => {
                self.usage = usage_from(&event["response"]["usage"]);
                return Some(Outcome::Completed);
            }
            "response.incomplete" => {
                self.usage = usage_from(&event["response"]["usage"]);
                let reason = event["response"]["incomplete_details"]["reason"].as_str().unwrap_or("unknown");
                return Some(Outcome::Failed(format!("ChatGPT stopped before finishing ({reason}).")));
            }
            "response.failed" => {
                let error = &event["response"]["error"];
                return Some(Outcome::Failed(friendly_error(error["code"].as_str(), error["message"].as_str(), None)));
            }
            "error" => {
                let error = if event["error"].is_object() { &event["error"] } else { event };
                return Some(Outcome::Failed(friendly_error(error["code"].as_str(), error["message"].as_str(), None)));
            }
            _ => {}
        }
        None
    }
}

/// Sends the request and feeds the stream to `stream` until it ends.
fn stream_response(body: &Value, stop: &AtomicBool, stream: &mut TurnStream) -> Result<Outcome> {
    let token = super::access_token()?;
    let mut response = agent()
        .post(RESPONSES_URL)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .send(serde_json::to_string(body)?)
        .context("contacting OpenAI")?;
    let status = response.status().as_u16();
    if status != 200 {
        let body = response.body_mut().read_to_string().unwrap_or_default();
        let json: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let error = &json["error"];
        let message = error["message"].as_str().or_else(|| json["detail"].as_str());
        return Ok(Outcome::Failed(friendly_error(error["code"].as_str(), message, Some(status))));
    }
    let reader = BufReader::new(response.into_body().into_reader());
    let mut data = String::new();
    for line in reader.lines() {
        if stop.load(Ordering::Relaxed) {
            return Ok(Outcome::Interrupted);
        }
        let line = line.context("reading ChatGPT's answer")?;
        if line.is_empty() {
            if let Some(outcome) = dispatch(&mut data, stream) {
                return Ok(outcome);
            }
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
    }
    if let Some(outcome) = dispatch(&mut data, stream) {
        return Ok(outcome);
    }
    Ok(Outcome::Failed("The connection to ChatGPT ended before the answer finished.".into()))
}

fn dispatch(data: &mut String, stream: &mut TurnStream) -> Option<Outcome> {
    if data.is_empty() {
        return None;
    }
    let payload = std::mem::take(data);
    if payload == "[DONE]" {
        return None;
    }
    let event: Value = serde_json::from_str(&payload).ok()?;
    stream.apply(&event)
}

/// Plain words for OpenAI's plan-usage errors.
fn friendly_error(code: Option<&str>, message: Option<&str>, status: Option<u16>) -> String {
    let detail = message.unwrap_or("no details");
    match code {
        Some("subscription_sharing_usage_limit_exceeded") => {
            format!("You've reached the usage limit for your ChatGPT plan or for Hangar. Manage usage at {USAGE_URL}.")
        }
        Some("subscription_sharing_user_not_eligible") => {
            "Your ChatGPT plan can't be used in other apps. Plus and Pro plans can.".into()
        }
        Some("subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable") => {
            "ChatGPT couldn't check your plan's usage just now. Try again in a moment.".into()
        }
        Some("subscription_sharing_unsupported_capability") => format!("ChatGPT can't do that here: {detail}"),
        Some("subscription_sharing_invalid_user") => {
            "ChatGPT didn't accept the sign-in. Sign out and sign in again from the Codex menu.".into()
        }
        _ => match status {
            Some(401) => "The ChatGPT sign-in was not accepted. Sign out and sign in again from the Codex menu.".into(),
            Some(429) => format!("ChatGPT is limiting requests: {detail}"),
            Some(status) => format!("ChatGPT returned an error ({status}): {detail}"),
            None => format!("ChatGPT failed: {detail}"),
        },
    }
}

fn usage_from(usage: &Value) -> Option<TokenUsageBreakdown> {
    let input = usage["input_tokens"].as_i64()?;
    let output = usage["output_tokens"].as_i64().unwrap_or(0);
    Some(TokenUsageBreakdown {
        total_tokens: usage["total_tokens"].as_i64().unwrap_or(input + output),
        input_tokens: input,
        cached_input_tokens: usage["input_tokens_details"]["cached_tokens"].as_i64().unwrap_or(0),
        cache_write_input_tokens: 0,
        output_tokens: output,
        reasoning_output_tokens: usage["output_tokens_details"]["reasoning_tokens"].as_i64().unwrap_or(0),
    })
}

/// The models the signed-in account can use, in OpenAI's order.
fn fetch_models() -> Result<Vec<Model>> {
    let token = super::access_token()?;
    let mut response = agent()
        .get(MODELS_URL)
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .context("contacting OpenAI")?;
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().unwrap_or_default();
    if status != 200 {
        bail!("listing ChatGPT models failed ({status})");
    }
    parse_models(&serde_json::from_str(&body).context("reading the ChatGPT model list")?)
}

fn parse_models(json: &Value) -> Result<Vec<Model>> {
    let list = json["models"].as_array().or_else(|| json["data"].as_array()).context("no model list")?;
    let mut models: Vec<Model> = list
        .iter()
        .filter(|m| m["visibility"].as_str().is_none_or(|v| v == "list"))
        .filter_map(|m| {
            let slug = m["slug"].as_str().or_else(|| m["id"].as_str())?.to_string();
            let efforts = m["supported_reasoning_levels"]
                .as_array()
                .map(|levels| {
                    levels
                        .iter()
                        .filter_map(|l| {
                            Some(ReasoningEffortOption {
                                reasoning_effort: l["effort"].as_str()?.to_string(),
                                description: l["description"].as_str().unwrap_or_default().to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Model {
                id: slug.clone(),
                display_name: m["display_name"].as_str().unwrap_or(&slug).to_string(),
                description: m["description"].as_str().unwrap_or_default().to_string(),
                default_reasoning_effort: m["default_reasoning_level"].as_str().unwrap_or_default().to_string(),
                supported_reasoning_efforts: efforts,
                model: slug,
                ..Model::default()
            })
        })
        .collect();
    if let Some(first) = models.first_mut() {
        first.is_default = true;
    }
    Ok(models)
}

/// A saved conversation as turns, for the transcript.
fn turns_from(thread_id: &str, messages: &[ChatMessage]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for (ix, message) in messages.iter().enumerate() {
        let id = format!("{thread_id}-{ix}");
        if message.role == "user" || turns.is_empty() {
            turns.push(Turn { id: format!("turn-{ix}"), status: TurnStatus::Completed, ..Turn::default() });
        }
        let item = if message.role == "user" {
            ThreadItem::UserMessage { id, content: vec![UserInput::text(message.text.clone())] }
        } else {
            ThreadItem::AgentMessage { id, text: message.text.clone() }
        };
        if let Some(turn) = turns.last_mut() {
            turn.items.push(item);
        }
    }
    turns
}

fn thread_path(id: &str) -> Result<PathBuf> {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        bail!("not a ChatGPT conversation id");
    }
    let dir = super::store::data_dir()?.join("chatgpt-threads");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{id}.json")))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(180)))
        .user_agent(format!("{}/{}", cockpit_common::APP_NAME, cockpit_common::APP_VERSION))
        .build()
        .into()
}

fn random_hex(bytes: usize) -> Result<String> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).map_err(|e| anyhow!("no randomness: {e}"))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream() -> (TurnStream, async_channel::Receiver<AgentEvent>) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (client, events) = ChatGptClient::new(runtime.handle().clone());
        std::mem::forget(runtime);
        (TurnStream::new(client, "chatgpt-1".into(), "turn-1".into()), events)
    }

    #[test]
    fn streams_text_and_thinking_as_items() {
        let (mut s, events) = stream();
        let feed = [
            json!({"type": "response.created"}),
            json!({"type": "response.reasoning_summary_text.delta", "delta": "Thinking"}),
            json!({"type": "response.output_text.delta", "delta": "Hel"}),
            json!({"type": "response.output_text.delta", "delta": "lo"}),
        ];
        for event in &feed {
            assert!(s.apply(event).is_none());
        }
        let done = s.apply(&json!({"type": "response.completed", "response": {"usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}}}));
        assert!(matches!(done, Some(Outcome::Completed)));
        s.finish();
        assert_eq!(s.text, "Hello");
        assert_eq!(s.usage.unwrap().total_tokens, 15);
        let kinds: Vec<&str> = std::iter::from_fn(|| events.try_recv().ok()).map(|e| e.kind()).collect();
        assert_eq!(
            kinds,
            [
                "item/started",
                "item/reasoning/delta",
                "item/started",
                "item/agentMessage/delta",
                "item/agentMessage/delta",
                "item/completed",
                "item/completed"
            ]
        );
    }

    #[test]
    fn failures_read_plainly() {
        let (mut s, _events) = stream();
        let failed = s.apply(&json!({"type": "response.failed", "response": {"error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "limit"}}}));
        assert!(matches!(failed, Some(Outcome::Failed(m)) if m.contains("usage limit") && m.contains(USAGE_URL)));
        assert!(friendly_error(None, Some("bad"), Some(401)).contains("sign in again"));
        assert_eq!(friendly_error(None, Some("boom"), Some(500)), "ChatGPT returned an error (500): boom");
    }

    #[test]
    fn sse_events_are_split_on_blank_lines() {
        let (mut s, _events) = stream();
        let mut data = String::new();
        data.push_str(r#"{"type":"response.output_text.delta","delta":"a"}"#);
        assert!(dispatch(&mut data, &mut s).is_none());
        assert!(data.is_empty());
        assert_eq!(s.text, "a");
        data.push_str("[DONE]");
        assert!(dispatch(&mut data, &mut s).is_none());
    }

    #[test]
    fn models_keep_listed_ones_in_order() {
        let json = json!({"models": [
            {"slug": "gpt-6.1-sol", "display_name": "GPT-6.1-Sol", "visibility": "list",
             "default_reasoning_level": "medium",
             "supported_reasoning_levels": [{"effort": "low", "description": "Fast"}, {"effort": "high", "description": "Deep"}]},
            {"slug": "hidden-one", "visibility": "hide"},
            {"slug": "gpt-6-luna", "display_name": "GPT-6 Luna"}
        ]});
        let models = parse_models(&json).unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-6.1-sol", "gpt-6-luna"]);
        assert!(models[0].is_default && !models[1].is_default);
        assert_eq!(models[0].supported_reasoning_efforts.len(), 2);
        assert_eq!(models[0].default_reasoning_effort, "medium");
    }

    #[test]
    fn saved_conversations_become_turns() {
        let messages = vec![
            ChatMessage { role: "user".into(), text: "hi".into() },
            ChatMessage { role: "assistant".into(), text: "hello".into() },
            ChatMessage { role: "user".into(), text: "again".into() },
        ];
        let turns = turns_from("chatgpt-1", &messages);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].items.len(), 2);
        assert!(matches!(&turns[1].items[0], ThreadItem::UserMessage { .. }));
        assert!(thread_path("../etc/passwd").is_err());
    }
}
