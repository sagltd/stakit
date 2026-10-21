//! The stateful agent: a session object holding the conversation, providers,
//! tools, skills, and middleware, plus the loop that drives them as an async
//! event stream.
//!
//! [`Agent`] is **stateful** and cheap to create — build one per request / cron
//! tick / sub-agent. [`Agent::run`] mutates `self.messages` in place and yields
//! a stream of [`AgentEvent`]s: it calls the model, streams its output, runs any
//! requested tools (gated by middleware [`on_tool_approve`](AgentMiddleware::on_tool_approve)),
//! appends the results, and repeats until the model ends its turn, a stop
//! condition fires, a middleware stops, or it is cancelled (drop the run / cancel
//! the token).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::stream::{BoxStream, Stream};
use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::agent_cx::AgentCx;
use crate::cache::CacheStrategy;
use crate::cancel::CancelToken;
use crate::control::{Approval, Flow};
use crate::cx::ToolCx;
use crate::error::AgentError;
use crate::loop_event::{
    AgentEvent, Finish, Outcome, PendingToolCall, Step, StopCond, ToolCallRecord, ToolOutcome,
};
use crate::message::{
    AssistantContent, Message, SystemPrompt, Thinking, ToolResultPart, UserContent,
};
use crate::middleware::AgentMiddleware;
use crate::provider::{
    ChatRequest, MAX_STREAM_BLOCK_BYTES, MAX_STREAM_RESPONSE_BLOCKS, MAX_STREAM_RESPONSE_BYTES,
    Provider, StopReason, StreamEvent, ToolChoice, ToolDef, assistant_content_bytes,
};
use crate::retry::RetryPolicy;
use crate::skill::{Skill, SkillLoader};
use crate::tool::{Tool, ToolDyn, TypedTool};
use crate::usage::{Pricing, Usage};

/// Name of the built-in skill-load tool (offered when a skill loader is set).
const LOAD_SKILL: &str = "load_skill";
/// Name of the built-in skill-search tool.
const SEARCH_SKILLS: &str = "search_skills";
/// Default cap on steps when no [`StopCond::StepCountIs`] is configured.
const DEFAULT_MAX_STEPS: u32 = 16;
/// Synthetic `tool_result` text for a `tool_use` cancelled by a mid-batch
/// `Approval::Stop`, so the persisted turn has no dangling `tool_use` block.
const CANCELLED_TOOL_RESULT: &str = "tool call cancelled: run stopped";

/// A closure producing a prompt-cache key from the app context.
type CacheKeyFn<Ctx> = Arc<dyn Fn(&Ctx) -> Option<Arc<str>> + Send + Sync>;

/// A completed provider response must explicitly authorize its queued tools.
const fn terminal_failure(stop: &StopReason, has_tools: bool) -> Option<&'static str> {
    match stop {
        StopReason::ToolUse if has_tools => None,
        StopReason::EndTurn | StopReason::StopSequence(_) if !has_tools => None,
        StopReason::Refusal => Some("provider refused the request"),
        StopReason::MaxTokens => Some("provider reached its output limit"),
        StopReason::Pause => Some("provider paused the turn"),
        StopReason::Other(_) => Some("provider returned an unsupported stop reason"),
        StopReason::ToolUse => Some("provider requested tools without any tool calls"),
        StopReason::EndTurn | StopReason::StopSequence(_) => {
            Some("provider stopped without authorizing its tool calls")
        }
    }
}

/// Signed history attests to this request context and its unchanged prefix.
/// Appending tool results and later turns is safe; rewriting the bound prefix is not.
struct ReplayBinding {
    model: String,
    system: Option<SystemPrompt>,
    tools: Arc<[ToolDef]>,
    messages: Vec<Message>,
}

impl ReplayBinding {
    fn from_request(request: &ChatRequest) -> Self {
        Self {
            model: request.model.clone(),
            system: request.system.clone(),
            tools: Arc::clone(&request.tools),
            messages: request.messages.clone(),
        }
    }

    fn accepts(&self, request: &ChatRequest) -> bool {
        self.model == request.model
            && self.system == request.system
            && self.tools == request.tools
            && request.messages.starts_with(&self.messages)
    }
}

const fn is_signed(content: &AssistantContent) -> bool {
    matches!(
        content,
        AssistantContent::Thinking(
            Thinking::Visible {
                signature: Some(_),
                ..
            } | Thinking::Redacted { .. }
        )
    )
}

fn has_signed_history(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        matches!(message,
        Message::Assistant(blocks) if blocks.iter().any(is_signed))
    })
}

/// A stateful agent session.
///
/// Holds the app context, the registered providers, the active model, the tools,
/// skills (name + description), middleware, the conversation, and telemetry.
/// Cheap to create (handles only); [`run`](Agent::run) mutates it.
pub struct Agent<Ctx> {
    ctx: Ctx,
    providers: IndexMap<String, Box<dyn Provider>>,
    current_model: String,
    tools: Vec<Box<dyn ToolDyn<Ctx>>>,
    /// Cached `name + description` after the first `list()`.
    skills: Vec<Skill>,
    /// The skill manifest, rendered once after `skills` are listed. The manifest
    /// is invariant for the run (skills are fixed once listed), so it is cached
    /// here rather than re-rendered every step in [`build_system`](Self::build_system).
    skill_manifest: Option<Arc<str>>,
    skill_loader: Option<Box<dyn SkillLoader<Ctx>>>,
    middleware: Vec<Box<dyn AgentMiddleware<Ctx>>>,
    system: Option<String>,
    messages: Vec<Message>,
    replay_binding: Option<ReplayBinding>,
    cache: CacheStrategy,
    cache_key: Option<CacheKeyFn<Ctx>>,
    retry: RetryPolicy,
    pricing: Pricing,
    usage: Usage,
    /// Sum of request costs; never reprice earlier requests when model/tier changes.
    total_cost: Option<f64>,
    max_tokens: u32,
    cancel: CancelToken,
}

impl<Ctx: Send + Sync + 'static> Agent<Ctx> {
    /// Creates a new agent over the app context `ctx`. Cheap — stores handles.
    ///
    /// The conversation starts empty; seed it with [`with_context`](Agent::with_context)
    /// or load it from a middleware [`on_start`](AgentMiddleware::on_start).
    pub fn new(ctx: Ctx) -> Self {
        Self {
            ctx,
            providers: IndexMap::new(),
            current_model: String::new(),
            tools: Vec::new(),
            skills: Vec::new(),
            skill_manifest: None,
            skill_loader: None,
            middleware: Vec::new(),
            system: None,
            messages: Vec::new(),
            replay_binding: None,
            cache: CacheStrategy::Auto,
            cache_key: None,
            retry: RetryPolicy::default(),
            pricing: Pricing::new(),
            usage: Usage::default(),
            total_cost: Some(0.0),
            max_tokens: 4096,
            cancel: CancelToken::new(),
        }
    }

    /// Registers `provider` and makes its model the default if none is set yet.
    #[must_use]
    pub fn provider(mut self, provider: impl Provider + 'static) -> Self {
        let model = provider.model_id().to_owned();
        if self.current_model.is_empty() {
            self.current_model.clone_from(&model);
        }
        self.providers.insert(model, Box::new(provider));
        self
    }

    /// Registers an additional provider (keyed by its `model_id()`).
    #[must_use]
    pub fn register_provider(mut self, provider: impl Provider + 'static) -> Self {
        self.providers
            .insert(provider.model_id().to_owned(), Box::new(provider));
        self
    }

    /// Sets the default (active) model id.
    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.current_model = model.into();
        self
    }

    /// Sets the base system prompt.
    #[must_use]
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Registers a typed tool.
    #[must_use]
    pub fn register_tool<T: Tool<Ctx>>(mut self, tool: T) -> Self {
        self.tools.push(Box::new(TypedTool(tool)));
        self
    }

    /// Registers many typed tools.
    #[must_use]
    pub fn register_tools<T: Tool<Ctx>, I: IntoIterator<Item = T>>(mut self, tools: I) -> Self {
        for tool in tools {
            self.tools.push(Box::new(TypedTool(tool)));
        }
        self
    }

    /// Registers a middleware (runs in registration order).
    #[must_use]
    pub fn register_middleware<M: AgentMiddleware<Ctx>>(mut self, middleware: M) -> Self {
        self.middleware.push(Box::new(middleware));
        self
    }

    /// Sets the skill loader. Skills (name + description) are listed once on the
    /// first run and injected into the system prompt; bodies load on demand via
    /// the built-in `load_skill` / `search_skills` tools (progressive disclosure).
    #[must_use]
    pub fn skills<L: SkillLoader<Ctx>>(mut self, loader: L) -> Self {
        self.skill_loader = Some(Box::new(loader));
        self
    }

    /// Seeds the conversation (e.g. the new turn).
    ///
    /// Signed or redacted history attests that the caller supplied the original
    /// model, system prompt and tools. The provider verifies its signatures.
    /// After `on_start` loads history, this agent freezes that context and prefix
    /// against later mutation; appending new turns remains supported.
    #[must_use]
    pub fn with_context(mut self, messages: Vec<Message>) -> Self {
        self.messages = messages;
        self
    }

    /// Sets the prompt-cache strategy.
    #[must_use]
    pub fn cache(mut self, cache: CacheStrategy) -> Self {
        self.cache = cache;
        self
    }

    /// Sets a closure deriving a prompt-cache key from the app context (e.g. the
    /// session id) — used by providers with key-routed caching.
    #[must_use]
    pub fn cache_key(
        mut self,
        f: impl Fn(&Ctx) -> Option<Arc<str>> + Send + Sync + 'static,
    ) -> Self {
        self.cache_key = Some(Arc::new(f));
        self
    }

    /// Sets the max retry attempts after the first try (default 2).
    #[must_use]
    pub const fn with_retries(mut self, n: u32) -> Self {
        self.retry.max_retries = n;
        self
    }

    /// Sets the per-attempt provider-call timeout.
    #[must_use]
    pub const fn with_timeout(mut self, d: Duration) -> Self {
        self.retry.timeout = d;
        self
    }

    /// Sets the pricing table (for per-step cost estimates).
    #[must_use]
    pub fn pricing(mut self, pricing: Pricing) -> Self {
        self.pricing = pricing;
        self
    }

    /// Sets the max tokens generated per step.
    #[must_use]
    pub const fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Appends a message to the conversation before a run.
    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// The active model id.
    #[must_use]
    pub fn current_model(&self) -> &str {
        &self.current_model
    }

    /// The conversation so far.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Accumulated cost, unknown if a consumed request or current model is unpriced.
    fn current_cost(&self) -> Option<f64> {
        self.total_cost
            .filter(|_| self.pricing.price(&self.current_model).is_some())
    }

    /// Meter a single request at its own model and prompt-size rate.
    fn record_usage(&mut self, usage: &Usage) -> Option<f64> {
        let cost = self.pricing.cost(&self.current_model, usage);
        self.total_cost = self.total_cost.zip(cost).map(|(total, step)| total + step);
        self.usage.merge(usage);
        cost
    }

    /// Keep interrupted text and usage, but never checkpoint uncompleted tool
    /// or signed-thinking blocks into a conversation that may be resumed.
    fn record_partial(&mut self, step: &StepAccum, final_text: &mut String) {
        let _ = self.record_usage(&step.partial_usage());
        if step.text.is_empty() {
            return;
        }
        self.messages
            .push(Message::Assistant(vec![AssistantContent::Text(Arc::from(
                step.text.as_str(),
            ))]));
        final_text.clone_from(&step.text);
    }

    /// Accumulated token usage across runs.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
    }

    /// The agent's cancellation token (a shared handle). Cancel it from another
    /// task to stop a run cooperatively: an in-flight provider call aborts, the
    /// loop ends with [`Finish::Cancelled`], and every started middleware's
    /// `on_finish` still runs (so usage gets recorded). Dropping the run also
    /// cancels, but skips `on_finish`. Once cancelled, the token stays
    /// cancelled — later runs on this agent stop immediately.
    #[must_use]
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Whether a provider is registered for `model`.
    #[cfg(test)]
    #[must_use]
    pub fn has_provider(&self, model: &str) -> bool {
        self.providers.contains_key(model)
    }

    /// Runs the full agentic loop, yielding a stream of [`AgentEvent`]s and
    /// mutating the conversation in place.
    ///
    /// The returned [`AgentRun`] is both a [`Stream`] of events and an
    /// [`IntoFuture`](std::future::IntoFuture) resolving to the final
    /// [`Outcome`]. Cancellation: cancel the agent's token, or drop the run.
    ///
    /// `Ctx: Clone` is required so each tool call can receive its own
    /// [`ToolCx`]; app contexts are handles (db pools, `Arc`s, ids) and clone
    /// cheaply.
    pub fn run(&mut self) -> AgentRun<'_>
    where
        Ctx: Clone,
    {
        AgentRun {
            stream: Box::pin(self.run_inner()),
        }
    }

    /// The loop body, as an `async_stream`. Borrow-split note: the middleware is
    /// moved out with `mem::take` into a local `mw` for the run's duration (so the
    /// loop can iterate it while mutably borrowing `self.messages`/`self.system`/…),
    /// then swapped back before the terminal event.
    #[expect(
        clippy::too_many_lines,
        reason = "the loop is one cohesive state machine; splitting it would obscure control flow"
    )]
    fn run_inner(&mut self) -> impl Stream<Item = AgentEvent> + Send + '_
    where
        Ctx: Clone,
    {
        async_stream::stream! {
            // Take the middleware out so the loop can iterate it while mutably
            // borrowing other fields of `self`.
            let mw = std::mem::take(&mut self.middleware);
            // How many middlewares had `on_start` run (so `on_finish` mirrors it).
            let mut started = 0usize;

            let mut total_steps: u32 = 0;
            let mut final_text = String::new();
            let mut keep_partial_text = false;
            // The terminal finish; every exit path below assigns it before use.
            let finish: Finish;

            macro_rules! cost_now {
                () => {
                    self.current_cost()
                };
            }

            // ── Skills: list once, cache, and inject the manifest ──────────
            if self.skills.is_empty() {
                if let Some(loader) = &self.skill_loader {
                    match loader.list(&self.ctx).await {
                        Ok(skills) => self.skills = skills,
                        Err(e) => {
                            finish = Finish::Stopped {
                                message: Some(format!("skill loader error: {e}")),
                            };
                            run_on_finish(&mw, started, self, total_steps).await;
                            yield AgentEvent::Done(make_outcome(
                                String::new(),
                                self.usage,
                                cost_now!(),
                                total_steps,
                                finish,
                            ));
                            self.middleware = mw;
                            return;
                        }
                    }
                }
            }
            let skills_active = !self.skills.is_empty();
            // Render the skill manifest once now that the skill list is known; it
            // is invariant for the rest of the run, so `build_system` reuses this
            // cached `Arc<str>` each step instead of re-rendering it.
            if skills_active && self.skill_manifest.is_none() {
                self.skill_manifest = Some(Arc::from(self.render_skill_manifest()));
            }

            // Tool defs and the cache key are constant for the whole run (tools
            // and skills are fixed once it starts; the cache key derives only
            // from the app context). Compute them once and clone the cheap pieces
            // into each step's request rather than recomputing `t.def()` every
            // step. The system prompt is recomputed each step so that a
            // middleware's `set_system` call in `on_step` takes effect immediately
            // on the very next provider request.
            let (run_tools, run_cache_key) =
                self.build_run_constants(skills_active);

            // ── on_start (registration order; first Stop halts) ────────────
            'run: {
                for m in &mw {
                    let mut cx = self.agent_cx(0, None);
                    started += 1;
                    match m.on_start(&mut cx).await {
                        Ok(Flow::Continue) => {}
                        Ok(Flow::Stop(msg)) => {
                            finish = Finish::Stopped { message: Some(msg.clone()) };
                            final_text = msg;
                            break 'run;
                        }
                        Err(e) => {
                            finish = Finish::Stopped {
                                message: Some(format!("on_start error: {e}")),
                            };
                            break 'run;
                        }
                    }
                }

                // Loading signed history is a caller-owned provenance boundary.
                // Once loading ends, later middleware must preserve its context.
                if self.replay_binding.is_none() && has_signed_history(&self.messages) {
                    self.replay_binding = Some(ReplayBinding::from_request(&self.build_request(
                        run_tools.clone(), self.build_system(), run_cache_key.clone(),
                    )));
                }

                // ── Step loop ──────────────────────────────────────────────
                let mut last_step: Option<Step> = None;
                loop {
                    if self.cancel.is_cancelled() {
                        finish = Finish::Cancelled;
                        break 'run;
                    }
                    let index = total_steps;

                    // on_step (first Stop halts).
                    let mut stop_requested: Option<Flow> = None;
                    for m in &mw {
                        let mut cx = self.agent_cx(index, last_step.as_ref());
                        match m.on_step(&mut cx).await {
                            Ok(Flow::Continue) => {}
                            other => {
                                stop_requested = Some(match other {
                                    Ok(f) => f,
                                    Err(e) => Flow::Stop(format!("on_step error: {e}")),
                                });
                                break;
                            }
                        }
                    }
                    if let Some(Flow::Stop(msg)) = stop_requested {
                        finish = Finish::Stopped { message: Some(msg.clone()) };
                        final_text = msg;
                        break 'run;
                    }

                    yield AgentEvent::StepStart { index };

                    // Build the request. Tool defs and cache key are pre-computed
                    // (constant for the run). The system prompt is rebuilt each
                    // step from `self.system` so that `set_system` calls from
                    // `on_step` take effect on the immediately following request.
                    let step_system = self.build_system();
                    let request = self.build_request(
                        run_tools.clone(),
                        step_system,
                        run_cache_key.clone(),
                    );
                    let Some(provider) = self.providers.get(&self.current_model) else {
                        finish = Finish::Stopped {
                            message: Some(
                                AgentError::context(format!(
                                    "no provider registered for model {:?}",
                                    self.current_model
                                ))
                                .to_string(),
                            ),
                        };
                        break 'run;
                    };

                    if let Err(error) = request.validate_cache(&provider.cache_capabilities_for(&request.model)) {
                        finish = Finish::Stopped { message: Some(format!("provider error: {error}")) };
                        break 'run;
                    }

                    if self.replay_binding.as_ref().is_some_and(|binding| !binding.accepts(&request)) {
                        keep_partial_text = true;
                        finish = Finish::Stopped {
                            message: Some("signed assistant context was changed; original model, system, tools and message prefix are required".into()),
                        };
                        break 'run;
                    }
                    let mut replay_context = ReplayBinding::from_request(&request);

                    // Acquire the provider stream with retry + per-attempt
                    // timeout (no events have been yielded yet, so retry is
                    // safe). Once events flow they are forwarded live below.
                    let (first, mut stream) = match acquire_stream(
                        provider.as_ref(),
                        request,
                        &self.retry,
                        &self.cancel,
                    )
                    .await
                    {
                        AcquireOutcome::Cancelled { prompt_tokens } => {
                            // A request already in flight may be billed: meter
                            // its estimated prompt (0 when none was sent).
                            let _ = self.record_usage(&Usage {
                                input_tokens: prompt_tokens,
                                ..Usage::default()
                            });
                            finish = Finish::Cancelled;
                            break 'run;
                        }
                        AcquireOutcome::Error(e) => {
                            finish = Finish::Stopped {
                                message: Some(format!("provider error: {e}")),
                            };
                            break 'run;
                        }
                        AcquireOutcome::Ready { first, stream } => (first, stream),
                    };

                    // Drain the stream, yielding each delta the instant it
                    // arrives while accumulating text/reasoning/tool calls for the
                    // step record + history. Cancellable mid-stream; no retry once
                    // started.
                    let mut acc = StepAccum::default();
                    let mut step_error: Option<crate::error::ProviderError> = None;
                    let mut cancelled = false;
                    let mut pending = Some(first);
                    loop {
                        let event = match pending.take() {
                            Some(ev) => ev,
                            None => {
                                tokio::select! {
                                    biased;
                                    () = self.cancel.cancelled() => {
                                        cancelled = true;
                                        break;
                                    }
                                    next = stream.next() => match next {
                                        None => break,
                                        Some(Ok(ev)) => ev,
                                        Some(Err(e)) => {
                                            step_error = Some(e);
                                            break;
                                        }
                                    }
                                }
                            }
                        };
                        if let Some(delta) = acc.apply(event) {
                            yield delta;
                        }
                        if acc.stop.is_some() || acc.protocol_error.is_some() {
                            break;
                        }
                    }

                    acc.capture_complete_text();

                    // A step cut short still used its prompt (and whatever it
                    // streamed): meter it, or stopping every reply early would
                    // spend without ever counting.
                    if cancelled {
                        self.record_partial(&acc, &mut final_text);
                        finish = Finish::Cancelled;
                        break 'run;
                    }
                    if let Some(e) = step_error {
                        self.record_partial(&acc, &mut final_text);
                        keep_partial_text = true;
                        finish = Finish::Stopped {
                            message: Some(format!("provider error: {e}")),
                        };
                        break 'run;
                    }

                    if let Some(message) = acc.validate_blocks() {
                        self.record_partial(&acc, &mut final_text);
                        keep_partial_text = true;
                        finish = Finish::Stopped { message: Some(message.into()) };
                        break 'run;
                    }
                    let Some(stop_reason) = acc.stop.take() else {
                        self.record_partial(&acc, &mut final_text);
                        keep_partial_text = true;
                        finish = Finish::Stopped {
                            message: Some("provider stream ended before its terminal event".into()),
                        };
                        break 'run;
                    };
                    let terminal_failure = terminal_failure(&stop_reason, !acc.tool_calls.is_empty());
                    if terminal_failure.is_some() {
                        acc.tool_calls.clear();
                        acc.reasoning.clear();
                        acc.saw_signature = false;
                        acc.complete_blocks.clear();
                    }
                    let StepAccum {
                        text,
                        reasoning,
                        tool_calls,
                        step_usage,
                        complete_blocks,
                        ..
                    } = acc;

                    let (text, reasoning) = if complete_blocks.is_empty() {
                        (text, reasoning)
                    } else {
                        let mut text = String::new();
                        let mut reasoning = String::new();
                        for block in complete_blocks.values() {
                            match block {
                                AssistantContent::Text(value) => text.push_str(value),
                                AssistantContent::Thinking(Thinking::Visible { text, .. }) => reasoning.push_str(text),
                                _ => {}
                            }
                        }
                        (text, reasoning)
                    };

                    // Convert the accumulated text/reasoning to `Arc<str>` ONCE,
                    // then share that `Arc` across the assistant block, the `Step`
                    // record, and the `StepEnd` event (clone the pointer, not the
                    // string).
                    let text_arc: Option<Arc<str>> =
                        (!text.is_empty()).then(|| Arc::from(text.as_str()));
                    let reasoning_arc: Option<Arc<str>> =
                        (!reasoning.is_empty()).then(|| Arc::from(reasoning.as_str()));

                    // Assemble the assistant turn (thinking → text → tool calls).
                    let mut blocks: Vec<AssistantContent> = Vec::new();
                    if let Some(reasoning) = &reasoning_arc {
                        blocks.push(AssistantContent::Thinking(Thinking::Visible {
                            text: Arc::clone(reasoning),
                            signature: None,
                        }));
                    }
                    if let Some(text) = &text_arc {
                        final_text = text.to_string();
                        blocks.push(AssistantContent::Text(Arc::clone(text)));
                    }
                    for call in &tool_calls {
                        blocks.push(AssistantContent::ToolUse {
                            id: call.id.as_str().into(),
                            name: call.name.as_str().into(),
                            input: call.args.clone(),
                        });
                    }
                    // Persist the assistant turn only when it has content. An empty
                    // step (no text/reasoning/tool calls) would otherwise push
                    // `Assistant(vec![])`, which an `on_step_done` checkpoint would
                    // persist — some providers 400 on resending an empty turn.
                    if !complete_blocks.is_empty() {
                        blocks = complete_blocks.into_values().collect();
                    }
                    let signed_response = blocks.iter().any(is_signed);
                    if !blocks.is_empty() {
                        self.messages.push(Message::Assistant(blocks));
                    }
                    if signed_response {
                        replay_context.messages.clone_from(&self.messages);
                        self.replay_binding = Some(replay_context);
                    }

                    let step_cost = self.record_usage(&step_usage);

                    // ── Tool calls ─────────────────────────────────────────
                    // Every `tool_use` id in this assistant turn, captured before
                    // the approval pass consumes `tool_calls`. A mid-batch `Stop`
                    // uses this to synthesize a `tool_result` for any call that did
                    // not run, so the persisted history never has a dangling
                    // `tool_use` (which Claude rejects on resume).
                    let all_tool_ids: Vec<String> =
                        tool_calls.iter().map(|c| c.id.clone()).collect();
                    // Pass 1 (sequential): resolve each call's approval, which
                    // borrows the agent (via `AgentCx`). A `Stop` halts further
                    // collection. This pass must finish — releasing the mutable
                    // borrow — before the concurrent dispatch below.
                    let mut tool_stop: Option<Option<String>> = None;
                    let mut plans: Vec<ToolPlan> = Vec::with_capacity(tool_calls.len());
                    for ParsedCall {
                        id,
                        name,
                        args,
                        parse_error,
                    } in tool_calls
                    {
                        let pending = PendingToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            args: args.clone(),
                        };
                        let approval = {
                            let cx = self.agent_cx(index, last_step.as_ref());
                            resolve_approval(&mw, &cx, &pending).await
                        };
                        match approval {
                            Ok(Approval::Stop { message }) => {
                                tool_stop = Some(message);
                                break;
                            }
                            Ok(Approval::Deny { message }) => plans.push(ToolPlan {
                                id,
                                name,
                                args,
                                approval: Approval::Deny { message: message.clone() },
                                denied: Some(message),
                                parse_error,
                            }),
                            Ok(Approval::Allow) => plans.push(ToolPlan {
                                id,
                                name,
                                args,
                                approval: Approval::Allow,
                                denied: None,
                                parse_error,
                            }),
                            Err(e) => {
                                tool_stop = Some(Some(format!("on_tool_approve error: {e}")));
                                break;
                            }
                        }
                    }

                    // Pass 2 (concurrent): dispatch every allowed call at once
                    // (the model may request several), preserving order. Denied
                    // calls resolve immediately to their message.
                    let executed = dispatch_plans(&*self, plans).await;

                    let had_tool_calls = !executed.is_empty();
                    let mut records: Vec<ToolCallRecord> = Vec::with_capacity(executed.len());
                    let mut results: Vec<UserContent> = Vec::with_capacity(executed.len());
                    for (plan, outcome, elapsed) in executed {
                        let (payload, is_error) = match &outcome {
                            ToolOutcome::Ok(v) => (v.clone(), false),
                            ToolOutcome::Denied { message } => (json!(message), true),
                            ToolOutcome::Error(msg) => (json!(msg), true),
                        };
                        results.push(UserContent::ToolResult {
                            id: plan.id.as_str().into(),
                            content: vec![ToolResultPart::Text(value_to_text(&payload).into())],
                            is_error,
                        });
                        yield AgentEvent::ToolResult {
                            id: plan.id.clone(),
                            name: plan.name.clone(),
                            result: outcome.clone(),
                        };
                        records.push(ToolCallRecord {
                            id: plan.id,
                            name: plan.name,
                            args: plan.args,
                            approval: plan.approval,
                            result: outcome,
                            elapsed,
                        });
                    }

                    // A mid-batch `Stop` leaves the calls at/after the stopping
                    // call without results. Append a synthetic "cancelled" result
                    // for every such `tool_use` id so the persisted assistant turn
                    // has a matching `tool_result` for each id (Claude rejects a
                    // resumed turn with dangling `tool_use` blocks).
                    if tool_stop.is_some() {
                        for id in &all_tool_ids {
                            if records.iter().any(|r| r.id == *id) {
                                continue;
                            }
                            results.push(UserContent::ToolResult {
                                id: id.as_str().into(),
                                content: vec![ToolResultPart::Text(CANCELLED_TOOL_RESULT.into())],
                                is_error: true,
                            });
                            yield AgentEvent::ToolResult {
                                id: id.clone(),
                                name: String::new(),
                                result: ToolOutcome::Error(CANCELLED_TOOL_RESULT.to_owned()),
                            };
                        }
                    }

                    if !results.is_empty() {
                        self.messages.push(Message::User(results));
                    }

                    // Build the step record and run on_step_done. Reuse the
                    // step's `Arc<str>` for both the record and the `StepEnd`
                    // event (empty text becomes a shared empty `Arc`).
                    let step_text: Arc<str> = text_arc.unwrap_or_else(|| Arc::from(""));
                    let step = Step {
                        index,
                        reasoning: reasoning_arc.clone(),
                        text: Arc::clone(&step_text),
                        tool_calls: records,
                        stop: stop_reason.clone(),
                    };
                    total_steps += 1;

                    yield AgentEvent::StepEnd {
                        index,
                        text: step_text,
                        reasoning: reasoning_arc,
                        usage: step_usage,
                        cost: step_cost,
                    };

                    last_step = Some(step);

                    let mut done_stop: Option<Flow> = None;
                    for m in &mw {
                        let mut cx = self.agent_cx(index, last_step.as_ref());
                        match m.on_step_done(&mut cx).await {
                            Ok(Flow::Continue) => {}
                            other => {
                                done_stop = Some(match other {
                                    Ok(f) => f,
                                    Err(e) => Flow::Stop(format!("on_step_done error: {e}")),
                                });
                                break;
                            }
                        }
                    }
                    if let Some(Flow::Stop(msg)) = done_stop {
                        finish = Finish::Stopped { message: Some(msg.clone()) };
                        final_text = msg;
                        break 'run;
                    }

                    // A tool-approval Stop halts after recording results.
                    if let Some(message) = tool_stop {
                        if let Some(m) = &message {
                            final_text.clone_from(m);
                        }
                        finish = Finish::Stopped { message };
                        break 'run;
                    }

                    if let Some(message) = terminal_failure {
                        keep_partial_text = true;
                        finish = Finish::Stopped { message: Some(message.into()) };
                        break 'run;
                    }

                    // No tool calls → the model ended its turn.
                    if !had_tool_calls {
                        finish = Finish::EndTurn;
                        break 'run;
                    }

                    // Default step cap (a configurable budget/step stop is a
                    // middleware concern; see the design doc).
                    if total_steps >= DEFAULT_MAX_STEPS {
                        finish = Finish::Limit(StopCond::StepCountIs(DEFAULT_MAX_STEPS));
                        break 'run;
                    }
                }
            }

            // ── on_finish (every middleware whose on_start ran) ────────────
            run_on_finish(&mw, started, self, total_steps).await;

            let text = match &finish {
                _ if keep_partial_text && !final_text.is_empty() => final_text,
                Finish::Stopped { message } => message.clone().unwrap_or_else(|| final_text.clone()),
                _ => final_text,
            };
            let cost = cost_now!();
            let outcome = make_outcome(text, self.usage, cost, total_steps, finish);
            self.middleware = mw;
            yield AgentEvent::Done(outcome);
        }
    }

    /// Constructs a fresh middleware context borrowing the agent's run state.
    fn agent_cx<'b>(&'b mut self, index: u32, last_step: Option<&'b Step>) -> AgentCx<'b, Ctx> {
        let cost = self.current_cost();
        AgentCx::new(
            &self.ctx,
            &mut self.messages,
            &mut self.current_model,
            &mut self.system,
            &self.usage,
            cost,
            index,
            last_step,
            &self.cancel,
        )
    }

    /// Computes the parts of the request that are constant for the whole run:
    /// the tool definitions and the cache-routing key. Tools and skills are
    /// fixed once the run starts; the cache key derives only from the app
    /// context. The system prompt is computed per-step (see
    /// [`build_system`](Self::build_system)) so middleware can update it mid-run.
    fn build_run_constants(&self, skills_active: bool) -> (Arc<[ToolDef]>, Option<Arc<str>>) {
        let mut tools: Vec<ToolDef> = self.tools.iter().map(|t| t.def()).collect();
        if skills_active {
            tools.extend(skill_tool_defs());
        }

        // Derive the per-conversation cache-routing key from the app context.
        let cache_key = self.cache_key.as_ref().and_then(|f| f(&self.ctx));

        (Arc::from(tools), cache_key)
    }

    /// Builds the effective system prompt for one step.
    ///
    /// Called per-step (not once before the loop) so that
    /// [`AgentCx::set_system`](crate::agent_cx::AgentCx::set_system) calls from
    /// `on_step` take effect immediately on the next provider request. The skill
    /// manifest is taken from the cached `skill_manifest` (rendered once), so only
    /// `self.system` is read fresh each step — a cheap concat when both are set.
    fn build_system(&self) -> Option<SystemPrompt> {
        match (self.system.as_deref(), self.skill_manifest.as_deref()) {
            (Some(base), None) => Some(base.to_owned()),
            (Some(base), Some(manifest)) => Some(format!("{base}\n\n{manifest}")),
            (None, Some(manifest)) => Some(manifest.to_owned()),
            (None, None) => None,
        }
        .map(SystemPrompt::from)
    }

    /// Builds the provider request for the current step from the run-constant
    /// pieces ([`build_run_constants`](Self::build_run_constants)) plus the
    /// per-step conversation snapshot. The constants are cheap to clone (tool
    /// defs and an `Arc`-backed system prompt / cache key); only the message
    /// history is cloned afresh each step.
    fn build_request(
        &self,
        tools: Arc<[ToolDef]>,
        system: Option<SystemPrompt>,
        cache_key: Option<Arc<str>>,
    ) -> ChatRequest {
        ChatRequest {
            model: self.current_model.clone(),
            system,
            messages: self.messages.clone(),
            tools,
            tool_choice: ToolChoice::Auto,
            max_tokens: self.max_tokens,
            temperature: None,
            stop_sequences: Vec::new(),
            thinking: crate::provider::ThinkingConfig::Off,
            cache: self.cache.clone(),
            cache_key,
            parallel_tool_calls: None,
            extra: serde_json::Map::new(),
        }
    }

    /// Renders the cached skills as a system-prompt manifest block. Called once
    /// per run (the result is cached in `skill_manifest`).
    fn render_skill_manifest(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::from(
            "## Available skills\nUse `load_skill(id)` to load a skill's full instructions, or \
             `search_skills(query)` to find one.\n",
        );
        for skill in &self.skills {
            let _ = writeln!(s, "- {} ({}): {}", skill.name, skill.id, skill.description);
        }
        s
    }

    /// Dispatches one approved tool call: built-ins first, then user tools.
    ///
    /// User tools run against a [`ToolCx`] holding a clone of the app context and
    /// the run's cancel token (so cooperative cancellation reaches the tool).
    async fn dispatch_tool(&self, name: &str, args: Value) -> ToolOutcome
    where
        Ctx: Clone,
    {
        if name == SEARCH_SKILLS {
            return ToolOutcome::Ok(self.run_search_skills(&args));
        }
        if name == LOAD_SKILL {
            return match self.run_load_skill(&args).await {
                Ok(v) => ToolOutcome::Ok(v),
                Err(e) => ToolOutcome::Error(e.to_string()),
            };
        }
        match self.tools.iter().find(|t| t.name() == name) {
            Some(tool) => {
                let cx = ToolCx::with_cancel(self.ctx.clone(), self.cancel.clone());
                match tool.call_json(&cx, args).await {
                    Ok(v) => ToolOutcome::Ok(v),
                    Err(e) => ToolOutcome::Error(e.message().to_owned()),
                }
            }
            None => ToolOutcome::Error(format!("unknown tool: {name}")),
        }
    }

    /// Runs the built-in `search_skills` over the cached skill list.
    fn run_search_skills(&self, args: &Value) -> Value {
        let q = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        let matches: Vec<Value> = self
            .skills
            .iter()
            .filter(|s| {
                format!("{} {} {}", s.id, s.name, s.description)
                    .to_lowercase()
                    .contains(&q)
            })
            .map(|s| json!({ "id": s.id, "name": s.name, "description": s.description }))
            .collect();
        json!({ "matches": matches })
    }

    /// Runs the built-in `load_skill`: fetches one skill body on demand.
    async fn run_load_skill(&self, args: &Value) -> Result<Value, AgentError> {
        let loader = self
            .skill_loader
            .as_ref()
            .ok_or_else(|| AgentError::Skill("no skill loader configured".into()))?;
        let id = args.get("id").and_then(Value::as_str).unwrap_or_default();
        // A missing/empty id is a model mistake, not a loader call — reject it so
        // the model retries instead of the loader receiving "".
        if id.is_empty() {
            return Err(AgentError::Skill("missing required argument: id".into()));
        }
        let content = loader.load(&self.ctx, id).await?;
        Ok(json!({ "body": content.body, "references": content.references }))
    }
}

/// Runs `on_finish` for the first `started` middlewares, in registration order.
async fn run_on_finish<Ctx: Send + Sync + 'static>(
    mw: &[Box<dyn AgentMiddleware<Ctx>>],
    started: usize,
    agent: &mut Agent<Ctx>,
    index: u32,
) {
    for m in mw.iter().take(started) {
        let cx = agent.agent_cx(index, None);
        // on_finish is terminal; an error here is best-effort (cannot change the
        // outcome at this point).
        let _ = m.on_finish(&cx).await;
    }
}

/// Builds the final [`Outcome`].
const fn make_outcome(
    text: String,
    usage: Usage,
    cost: Option<f64>,
    steps: u32,
    finish: Finish,
) -> Outcome {
    Outcome {
        text,
        usage,
        cost,
        steps,
        finish,
    }
}

/// Max tool calls dispatched concurrently within one step. The model may batch
/// many calls; this caps in-flight work (and thus host load) while
/// [`buffered`](futures::stream::StreamExt::buffered) preserves result order.
const MAX_CONCURRENT_TOOLS: usize = 8;

/// Dispatches a step's tool plans with bounded concurrency, preserving order.
///
/// Denied calls resolve immediately to their message; calls whose arguments
/// failed to parse short-circuit to a [`ToolOutcome::Error`] without invoking
/// the tool. Everything else runs against the agent's context.
async fn dispatch_plans<Ctx: Clone + Send + Sync + 'static>(
    agent: &Agent<Ctx>,
    plans: Vec<ToolPlan>,
) -> Vec<(ToolPlan, ToolOutcome, Duration)> {
    futures::stream::iter(plans.into_iter().map(|plan| async move {
        let (outcome, elapsed) = if let Some(message) = &plan.denied {
            (
                ToolOutcome::Denied {
                    message: message.clone(),
                },
                Duration::ZERO,
            )
        } else if let Some(message) = &plan.parse_error {
            // Malformed arguments never reach the tool — surface the parse error
            // so the model can correct and retry.
            (ToolOutcome::Error(message.clone()), Duration::ZERO)
        } else {
            let start = Instant::now();
            let outcome = agent.dispatch_tool(&plan.name, plan.args.clone()).await;
            (outcome, start.elapsed())
        };
        (plan, outcome, elapsed)
    }))
    .buffered(MAX_CONCURRENT_TOOLS)
    .collect()
    .await
}

/// A tool call with its resolved approval, awaiting concurrent dispatch.
struct ToolPlan {
    id: String,
    name: String,
    args: Value,
    approval: Approval,
    /// `Some(message)` if denied (skip dispatch); `None` if allowed.
    denied: Option<String>,
    /// `Some(message)` if the model's arguments were malformed JSON; such a call
    /// short-circuits to a [`ToolOutcome::Error`] instead of being dispatched.
    parse_error: Option<String>,
}

/// Resolves the tool approval across all middleware (Stop > Deny > Allow).
async fn resolve_approval<Ctx: Send + Sync + 'static>(
    mw: &[Box<dyn AgentMiddleware<Ctx>>],
    cx: &AgentCx<'_, Ctx>,
    pending: &PendingToolCall,
) -> Result<Approval, AgentError> {
    let mut decision = Approval::Allow;
    for m in mw {
        match m.on_tool_approve(cx, pending).await? {
            Approval::Allow => {}
            Approval::Deny { message } => {
                // Deny only upgrades from Allow (Stop stays).
                if matches!(decision, Approval::Allow) {
                    decision = Approval::Deny { message };
                }
            }
            Approval::Stop { message } => return Ok(Approval::Stop { message }),
        }
    }
    Ok(decision)
}

/// One tool call from a step, with its arguments parsed once at the dispatch
/// boundary. `parse_error` is `Some` when the model emitted malformed JSON; the
/// loop short-circuits such a call to a [`ToolOutcome::Error`] so the model can
/// retry, rather than silently invoking the tool with empty arguments.
struct ParsedCall {
    id: String,
    name: String,
    /// Parsed arguments (an empty object when `parse_error` is set).
    args: Value,
    /// The parse error message, if the raw arguments were not valid JSON.
    parse_error: Option<String>,
}

impl ParsedCall {
    /// Parses raw tool-call argument text once, capturing any error.
    fn parse(id: String, name: String, arguments: &str) -> Self {
        match serde_json::from_str::<Value>(arguments) {
            Ok(args) => Self {
                id,
                name,
                args,
                parse_error: None,
            },
            Err(e) => Self {
                id,
                name,
                args: json!({}),
                parse_error: Some(format!("malformed tool arguments: {e}")),
            },
        }
    }
}

/// Accumulates one step's streamed output. `apply` folds each provider
/// [`StreamEvent`] into the running totals and returns the host-facing
/// [`AgentEvent`] to yield immediately (if any), so deltas reach the caller live
/// rather than buffered until the step ends.
#[derive(Default)]
struct StepAccum {
    text: String,
    reasoning: String,
    saw_signature: bool,
    tool_calls: Vec<ParsedCall>,
    step_usage: Usage,
    stop: Option<StopReason>,
    complete_blocks: BTreeMap<u64, AssistantContent>,
    complete_bytes: usize,
    delta_bytes: usize,
    protocol_error: Option<&'static str>,
}

impl StepAccum {
    /// Folds one event into the accumulator, returning a delta to yield live.
    fn apply(&mut self, event: StreamEvent) -> Option<AgentEvent> {
        let delta_bytes = match &event {
            StreamEvent::TextDelta(text)
            | StreamEvent::ReasoningDelta(text)
            | StreamEvent::SignatureDelta(text) => text.len(),
            StreamEvent::ToolCall {
                id,
                name,
                arguments,
            } => id
                .len()
                .saturating_add(name.len())
                .saturating_add(arguments.len()),
            _ => 0,
        };
        self.delta_bytes = self.delta_bytes.saturating_add(delta_bytes);
        if delta_bytes > MAX_STREAM_BLOCK_BYTES || self.delta_bytes > MAX_STREAM_RESPONSE_BYTES {
            self.protocol_error = Some("provider response exceeded its content limit");
            return None;
        }
        match event {
            StreamEvent::Start { usage } | StreamEvent::Usage { usage } => {
                self.step_usage = usage;
                None
            }
            StreamEvent::TextDelta(t) => {
                self.text.push_str(&t);
                Some(AgentEvent::MessageDelta(t))
            }
            StreamEvent::ReasoningDelta(t) => {
                self.reasoning.push_str(&t);
                Some(AgentEvent::ReasoningDelta(t))
            }
            StreamEvent::SignatureDelta(_) => {
                // Unindexed fragments cannot safely be attributed to blocks.
                // Complete blocks carry exact signatures; this is only a marker.
                self.saw_signature = true;
                None
            }
            StreamEvent::AssistantBlock { index, content } => {
                let bytes = assistant_content_bytes(&content);
                self.complete_bytes = self.complete_bytes.saturating_add(bytes);
                if bytes > MAX_STREAM_BLOCK_BYTES
                    || self.complete_bytes > MAX_STREAM_RESPONSE_BYTES
                    || self.complete_blocks.len() >= MAX_STREAM_RESPONSE_BLOCKS
                    || self.complete_blocks.contains_key(&index)
                {
                    self.protocol_error =
                        Some("provider returned duplicate or oversized assistant blocks");
                } else {
                    self.complete_blocks.insert(index, content);
                }
                None
            }
            StreamEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                if self.tool_calls.len() >= MAX_STREAM_RESPONSE_BLOCKS {
                    self.protocol_error = Some("provider returned too many tool calls");
                    return None;
                }
                let call = ParsedCall::parse(id.clone(), name.clone(), &arguments);
                let args = call.args.clone();
                self.tool_calls.push(call);
                Some(AgentEvent::ToolCall { id, name, args })
            }
            StreamEvent::End { stop, usage } => {
                self.stop = Some(stop);
                self.step_usage = usage;
                None
            }
        }
    }

    /// Complete-only providers need no duplicate text deltas. On interruption,
    /// keep their useful text while withholding incomplete tools and reasoning.
    fn capture_complete_text(&mut self) {
        if self.text.is_empty() {
            for block in self.complete_blocks.values() {
                if let AssistantContent::Text(text) = block {
                    self.text.push_str(text);
                }
            }
        }
        if self.reasoning.is_empty() {
            for block in self.complete_blocks.values() {
                if let AssistantContent::Thinking(Thinking::Visible { text, .. }) = block {
                    self.reasoning.push_str(text);
                }
            }
        }
    }

    /// Complete blocks are authoritative replay data, never reconstructed from
    /// unindexed reasoning/signature deltas. A partial protocol cannot run tools.
    fn validate_blocks(&mut self) -> Option<&'static str> {
        if self.protocol_error.is_some() {
            return self.protocol_error;
        }
        if self.saw_signature && !self.complete_blocks.values().any(is_signed) {
            return Some("signed provider output requires complete assistant blocks");
        }
        if self.complete_blocks.is_empty() {
            return None;
        }
        for (expected, index) in (0_u64..).zip(self.complete_blocks.keys()) {
            if expected != *index {
                return Some("provider returned incomplete assistant block indices");
            }
        }
        let mut ordered_calls = Vec::new();
        for block in self.complete_blocks.values() {
            if let AssistantContent::ToolUse { id, name, input } = block {
                if ordered_calls
                    .iter()
                    .any(|call: &ParsedCall| call.id == **id)
                {
                    return Some("provider returned duplicate tool call ids");
                }
                let Some(position) = self.tool_calls.iter().position(|call| {
                    call.id == **id
                        && call.name == **name
                        && call.args == *input
                        && call.parse_error.is_none()
                }) else {
                    return Some("complete assistant tool block does not match its tool call");
                };
                ordered_calls.push(self.tool_calls.remove(position));
            }
        }
        if !self.tool_calls.is_empty() {
            // Retain no partial complete-block protocol in resumable history.
            return Some("provider omitted complete assistant tool blocks");
        }
        self.tool_calls = ordered_calls;
        None
    }

    /// What a step cut short (stopped, or failed mid-stream) used: the
    /// provider's counts so far, with the output estimated from the streamed
    /// text when the provider has not reported that much yet.
    fn partial_usage(&self) -> Usage {
        let streamed = self.text.len().saturating_add(self.reasoning.len());
        Usage {
            output_tokens: self
                .step_usage
                .output_tokens
                .max(estimated_tokens(streamed)),
            ..self.step_usage
        }
    }
}

/// Reuse the production replay checks in the offline provider contract, so a
/// provider cannot pass conformance with blocks the agent would reject.
pub(crate) fn validate_stream_replay(events: &[StreamEvent]) -> Option<&'static str> {
    let mut step = StepAccum::default();
    for event in events {
        let _ = step.apply(event.clone());
        if step.protocol_error.is_some() {
            return step.protocol_error;
        }
    }
    step.validate_blocks()
}

/// Bytes per token in the rough estimates used where a provider reported no
/// usage (prose averages about four).
const BYTES_PER_TOKEN: usize = 4;

/// About how many tokens `bytes` of text are.
fn estimated_tokens(bytes: usize) -> u64 {
    u64::try_from(bytes.div_ceil(BYTES_PER_TOKEN)).unwrap_or(u64::MAX)
}

/// The bytes of text one message carries (tool calls as their JSON).
fn message_bytes(message: &Message) -> usize {
    match message {
        Message::User(blocks) => blocks
            .iter()
            .map(|block| match block {
                UserContent::Text(text) => text.len(),
                UserContent::ToolResult { content, .. } => content
                    .iter()
                    .map(|part| match part {
                        ToolResultPart::Text(text) => text.len(),
                        ToolResultPart::Image(_) => 0,
                    })
                    .sum(),
                UserContent::Image(_) | UserContent::Document(_) => 0,
            })
            .sum(),
        Message::Assistant(blocks) => blocks
            .iter()
            .map(|block| match block {
                AssistantContent::Text(text)
                | AssistantContent::Thinking(Thinking::Visible { text, .. }) => text.len(),
                AssistantContent::ToolUse { name, input, .. } => {
                    name.len() + input.to_string().len()
                }
                AssistantContent::Thinking(Thinking::Redacted { data }) => data.len(),
            })
            .sum(),
    }
}

/// A rough count of the input tokens `request` costs (system prompt, history,
/// tool definitions; images uncounted), for a request stopped while in flight:
/// the provider may already be processing — and billing — its prompt.
fn estimated_prompt_tokens(request: &ChatRequest) -> u64 {
    let system = request
        .system
        .as_ref()
        .map_or(0, |system| system.text.len());
    let history: usize = request.messages.iter().map(message_bytes).sum();
    let tools: usize = request
        .tools
        .iter()
        .map(|tool| tool.name.len() + tool.description.len() + tool.parameters.to_string().len())
        .sum();
    estimated_tokens(system + history + tools)
}

/// The result of trying to acquire a provider stream + its first event.
enum AcquireOutcome {
    Ready {
        first: StreamEvent,
        stream: crate::provider::EventStream,
    },
    Error(crate::error::ProviderError),
    /// Cancelled; `prompt_tokens` estimates the prompt of a request that was
    /// already in flight (0 when none was).
    Cancelled {
        prompt_tokens: u64,
    },
}

/// Tries up to `max_retries + 1` times to open the stream and read its first
/// event within `timeout` each attempt; retries transient/rate-limited failures
/// with backoff. No tokens are emitted before this returns.
async fn acquire_stream(
    provider: &dyn Provider,
    mut request: ChatRequest,
    retry: &RetryPolicy,
    cancel: &CancelToken,
) -> AcquireOutcome {
    request.fit_to(&provider.capabilities_for(&request.model));
    let attempts = retry.max_retries.saturating_add(1);
    let mut last_err: Option<crate::error::ProviderError> = None;
    // The classification of the most recent failure decides the next delay (so a
    // 429's `Retry-After` is honored before the retry that follows it).
    let mut last_class = crate::retry::Retryable::Transient;

    for attempt in 0..attempts {
        if cancel.is_cancelled() {
            return AcquireOutcome::Cancelled { prompt_tokens: 0 };
        }
        if attempt > 0 {
            let delay = retry.retry_delay(attempt - 1, &last_class);
            tokio::select! {
                biased;
                () = cancel.cancelled() => return AcquireOutcome::Cancelled { prompt_tokens: 0 },
                () = tokio::time::sleep(delay) => {}
            }
        }

        let req = request.clone();
        let attempt_fut = async {
            let mut stream = provider.stream(req).await?;
            // A failed first event is a failed attempt, classified like an open error.
            let first = stream.next().await.transpose()?;
            Ok::<_, crate::error::ProviderError>((first, stream))
        };

        let timed = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                return AcquireOutcome::Cancelled {
                    prompt_tokens: estimated_prompt_tokens(&request),
                };
            }
            r = tokio::time::timeout(retry.timeout, attempt_fut) => r,
        };

        match timed {
            // Timed out obtaining the stream / first event → transient.
            Err(_elapsed) => {
                last_err = Some(crate::error::ProviderError::Transport(
                    "provider call timed out".into(),
                ));
                last_class = crate::retry::Retryable::Transient;
            }
            Ok(Err(e)) => {
                let class = crate::retry::classify(&e);
                let retryable = retry.is_retryable(&class);
                last_err = Some(e);
                last_class = class;
                if !retryable {
                    break;
                }
            }
            Ok(Ok((None, _))) => {
                // An accepted but empty response is incomplete, never an
                // implicit success or an automatically retried safety turn.
                return AcquireOutcome::Error(crate::error::ProviderError::Transport(
                    "provider stream ended before its terminal event".into(),
                ));
            }
            Ok(Ok((Some(first), stream))) => return AcquireOutcome::Ready { first, stream },
        }
    }

    AcquireOutcome::Error(last_err.unwrap_or_else(|| {
        crate::error::ProviderError::Transport("provider produced no events".into())
    }))
}

fn value_to_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}

/// Built-in skill tool definitions, offered when a skill loader is set.
fn skill_tool_defs() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            LOAD_SKILL,
            "Load a skill's full instructions by id (from the available-skills list).",
            json!({
                "type": "object",
                "properties": { "id": { "type": "string", "description": "Skill id" } },
                "required": ["id"]
            }),
        ),
        ToolDef::new(
            SEARCH_SKILLS,
            "Search available skills by keyword; returns matching ids, names and descriptions.",
            json!({
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }),
        ),
    ]
}

/// A running agent loop: a stream of [`AgentEvent`]s that also resolves to the
/// final [`Outcome`] when awaited.
pub struct AgentRun<'a> {
    stream: BoxStream<'a, AgentEvent>,
}

impl AgentRun<'_> {
    /// Drives the run to completion, returning its [`Outcome`].
    ///
    /// # Errors
    /// Currently infallible at this layer — terminal errors are reported via
    /// [`Outcome::finish`] / [`Outcome::text`]. The `Result` is kept for forward
    /// compatibility (future fatal paths) and `?` ergonomics.
    pub async fn outcome(mut self) -> Result<Outcome, AgentError> {
        let mut last: Option<Outcome> = None;
        while let Some(ev) = self.stream.next().await {
            if let AgentEvent::Done(o) = ev {
                last = Some(o);
            }
        }
        Ok(last.unwrap_or_else(|| Outcome {
            text: String::new(),
            usage: Usage::default(),
            cost: None,
            steps: 0,
            finish: Finish::EndTurn,
        }))
    }
}

impl Stream for AgentRun<'_> {
    type Item = AgentEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.stream.as_mut().poll_next(cx)
    }
}

impl<'a> std::future::IntoFuture for AgentRun<'a> {
    type Output = Result<Outcome, AgentError>;
    type IntoFuture = futures::future::BoxFuture<'a, Result<Outcome, AgentError>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.outcome())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use futures::future::BoxFuture;

    use super::*;
    use crate::provider::{ChatResponse, EventStream};

    // Retry behaviour (transient retries, the retry cap) is covered end to end
    // by `tests/retry.rs`.
    #[test]
    fn provider_registers_and_sets_default_model() {
        let agent = Agent::new(()).provider(crate::provider::tests::MockProvider);
        assert!(agent.has_provider("mock"));
        assert!(!agent.has_provider("missing"));
        assert_eq!(agent.current_model(), "mock");
    }

    /// A provider that streams one delta and then never finishes.
    struct HangingProvider;

    impl Provider for HangingProvider {
        #[allow(
            clippy::unnecessary_literal_bound,
            reason = "trait method must return &str"
        )]
        fn model_id(&self) -> &str {
            "hanging"
        }
        fn complete(
            &self,
            _r: ChatRequest,
        ) -> BoxFuture<'_, Result<ChatResponse, crate::error::ProviderError>> {
            Box::pin(async { Err(crate::error::ProviderError::Cancelled) })
        }
        fn stream(
            &self,
            _r: ChatRequest,
        ) -> BoxFuture<'_, Result<EventStream, crate::error::ProviderError>> {
            Box::pin(async {
                let first = futures::stream::iter([Ok(StreamEvent::TextDelta("partial".into()))]);
                Ok(first.chain(futures::stream::pending()).boxed())
            })
        }
    }

    /// Records whether `on_finish` ran.
    struct FinishFlag(Arc<std::sync::atomic::AtomicBool>);

    #[async_trait::async_trait]
    impl AgentMiddleware<()> for FinishFlag {
        async fn on_finish(&self, _cx: &AgentCx<'_, ()>) -> Result<(), AgentError> {
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancel_token_stops_a_running_agent_and_still_runs_on_finish() {
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut agent = Agent::new(())
            .provider(HangingProvider)
            .register_middleware(FinishFlag(Arc::clone(&finished)))
            .with_context(vec![Message::user("hi")]);
        let token = agent.cancel_token();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            token.cancel();
        });

        let out = agent.run().await.expect("outcome");

        assert!(matches!(out.finish, Finish::Cancelled));
        assert!(finished.load(Ordering::SeqCst), "on_finish must run");
    }
}
