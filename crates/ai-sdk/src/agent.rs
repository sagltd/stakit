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
use crate::provider::{ChatRequest, Provider, StopReason, StreamEvent, ToolChoice, ToolDef};
use crate::retry::RetryPolicy;
use crate::skill::{Skill, SkillLoader};
use crate::tool::{Tool, ToolDyn, TypedTool};
use crate::usage::{Pricing, Usage};

/// Name of the built-in tool-search tool (offered when deferred tools exist).
const TOOL_SEARCH: &str = "tool_search";
/// Name of the built-in skill-load tool (offered when a skill loader is set).
const LOAD_SKILL: &str = "load_skill";
/// Name of the built-in skill-search tool.
const SEARCH_SKILLS: &str = "search_skills";
/// Default cap on steps when no [`StopCond::StepCountIs`] is configured.
const DEFAULT_MAX_STEPS: u32 = 16;

/// A closure producing a prompt-cache key from the app context.
type CacheKeyFn<Ctx> = Arc<dyn Fn(&Ctx) -> Option<Arc<str>> + Send + Sync>;

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
    skill_loader: Option<Box<dyn SkillLoader<Ctx>>>,
    middleware: Vec<Box<dyn AgentMiddleware<Ctx>>>,
    system: Option<String>,
    messages: Vec<Message>,
    cache: CacheStrategy,
    cache_key: Option<CacheKeyFn<Ctx>>,
    retry: RetryPolicy,
    pricing: Pricing,
    usage: Usage,
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
            skill_loader: None,
            middleware: Vec::new(),
            system: None,
            messages: Vec::new(),
            cache: CacheStrategy::Auto,
            cache_key: None,
            retry: RetryPolicy::default(),
            pricing: Pricing::new(),
            usage: Usage::default(),
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

    /// Accumulated token usage across runs.
    #[must_use]
    pub const fn usage(&self) -> &Usage {
        &self.usage
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
            // The terminal finish; every exit path below assigns it before use.
            let finish: Finish;

            macro_rules! cost_now {
                () => {
                    self.pricing.cost(&self.current_model, &self.usage)
                };
            }

            // ── Skills: list once, cache, and inject the manifest ──────────
            if self.skill_loader.is_some() && self.skills.is_empty() {
                let loader = self.skill_loader.as_ref().expect("checked is_some");
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
            let skills_active = !self.skills.is_empty();

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
                    if let Some(flow) = stop_requested {
                        let Flow::Stop(msg) = flow else { unreachable!() };
                        finish = Finish::Stopped { message: Some(msg.clone()) };
                        final_text = msg;
                        break 'run;
                    }

                    yield AgentEvent::StepStart { index };

                    // Build the request and resolve the provider.
                    let request = self.build_request(skills_active);
                    if !self.providers.contains_key(&self.current_model) {
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
                    }
                    let provider = self
                        .providers
                        .get(&self.current_model)
                        .expect("checked contains_key");

                    // Obtain the stream with retry + per-attempt timeout, then
                    // drain it (cancellable), accumulating output.
                    let outcome = stream_step(
                        provider.as_ref(),
                        request,
                        &self.retry,
                        &self.cancel,
                    )
                    .await;

                    let StepStreamResult {
                        first_error,
                        cancelled,
                        text,
                        reasoning,
                        signature,
                        tool_calls,
                        step_usage,
                        stop,
                        deltas,
                    } = outcome;

                    for ev in deltas {
                        yield ev;
                    }
                    if cancelled {
                        finish = Finish::Cancelled;
                        break 'run;
                    }
                    if let Some(e) = first_error {
                        finish = Finish::Stopped {
                            message: Some(format!("provider error: {e}")),
                        };
                        break 'run;
                    }

                    // Assemble the assistant turn (thinking → text → tool calls).
                    let mut blocks: Vec<AssistantContent> = Vec::new();
                    let reasoning_opt =
                        if reasoning.is_empty() { None } else { Some(reasoning.clone()) };
                    if !reasoning.is_empty() {
                        blocks.push(AssistantContent::Thinking(Thinking::Visible {
                            text: reasoning,
                            signature,
                        }));
                    }
                    if !text.is_empty() {
                        final_text = text.clone();
                        blocks.push(AssistantContent::Text(text.clone().into()));
                    }
                    for (id, name, args) in &tool_calls {
                        blocks.push(AssistantContent::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: args.clone(),
                        });
                    }
                    self.messages.push(Message::Assistant(blocks));

                    self.usage.merge(&step_usage);
                    let step_cost = self.pricing.cost(&self.current_model, &step_usage);

                    // ── Tool calls ─────────────────────────────────────────
                    // Pass 1 (sequential): resolve each call's approval, which
                    // borrows the agent (via `AgentCx`). A `Stop` halts further
                    // collection. This pass must finish — releasing the mutable
                    // borrow — before the concurrent dispatch below.
                    let mut tool_stop: Option<Option<String>> = None;
                    let mut plans: Vec<ToolPlan> = Vec::with_capacity(tool_calls.len());
                    for (id, name, args) in tool_calls {
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
                            }),
                            Ok(Approval::Allow) => plans.push(ToolPlan {
                                id,
                                name,
                                args,
                                approval: Approval::Allow,
                                denied: None,
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
                    let executed = futures::future::join_all(plans.into_iter().map(|plan| {
                        let agent = &*self;
                        async move {
                            let (outcome, elapsed) = if let Some(message) = &plan.denied {
                                (
                                    ToolOutcome::Denied { message: message.clone() },
                                    Duration::ZERO,
                                )
                            } else {
                                let start = Instant::now();
                                let outcome =
                                    agent.dispatch_tool(&plan.name, plan.args.clone()).await;
                                (outcome, start.elapsed())
                            };
                            (plan, outcome, elapsed)
                        }
                    }))
                    .await;

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
                            id: plan.id.clone(),
                            content: vec![ToolResultPart::Text(value_to_text(&payload))],
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

                    if !results.is_empty() {
                        self.messages.push(Message::User(results));
                    }

                    // Build the step record and run on_step_done.
                    let step = Step {
                        index,
                        reasoning: reasoning_opt.clone(),
                        text: text.clone(),
                        tool_calls: records,
                        stop: stop.clone(),
                    };
                    total_steps += 1;

                    yield AgentEvent::StepEnd {
                        index,
                        text,
                        reasoning: reasoning_opt,
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
        let cost = self.pricing.cost(&self.current_model, &self.usage);
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

    /// Builds the provider request for the current step.
    fn build_request(&self, skills_active: bool) -> ChatRequest {
        let mut tools: Vec<ToolDef> = self.tools.iter().map(|t| t.def()).collect();
        tools.push(tool_search_def());
        if skills_active {
            tools.extend(skill_tool_defs());
        }

        // Effective system prompt = base + skill manifest (if any). Either part
        // may be absent.
        let system = match (self.system.as_deref(), skills_active) {
            (Some(base), false) => Some(base.to_owned()),
            (Some(base), true) => Some(format!("{base}\n\n{}", self.skill_manifest())),
            (None, true) => Some(self.skill_manifest()),
            (None, false) => None,
        }
        .map(SystemPrompt::from);

        // Derive the per-conversation cache-routing key from the app context.
        let cache_key = self
            .cache_key
            .as_ref()
            .and_then(|f| f(&self.ctx))
            .map(|a| a.to_string());

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
            extra: serde_json::Map::new(),
        }
    }

    /// Renders the cached skills as a system-prompt manifest block.
    fn skill_manifest(&self) -> String {
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
        if name == TOOL_SEARCH {
            // Tool search is a no-op here (no deferred registry in the stateful
            // agent); report no matches so the model can proceed.
            return ToolOutcome::Ok(json!({ "matches": [] }));
        }
        if name == SEARCH_SKILLS {
            return ToolOutcome::Ok(self.run_search_skills(&args));
        }
        if name == LOAD_SKILL {
            return match self.run_load_skill(&args).await {
                Ok(v) => ToolOutcome::Ok(v),
                Err(e) => ToolOutcome::Error(e.to_string()),
            };
        }
        match self.tools.iter().find(|t| t.def().name == name) {
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

/// A tool call with its resolved approval, awaiting concurrent dispatch.
struct ToolPlan {
    id: String,
    name: String,
    args: Value,
    approval: Approval,
    /// `Some(message)` if denied (skip dispatch); `None` if allowed.
    denied: Option<String>,
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

/// Accumulated result of streaming one step.
struct StepStreamResult {
    first_error: Option<crate::error::ProviderError>,
    cancelled: bool,
    text: String,
    reasoning: String,
    signature: Option<String>,
    tool_calls: Vec<(String, String, Value)>,
    step_usage: Usage,
    stop: StopReason,
    /// Events to forward to the caller (deltas + tool calls), in order.
    deltas: Vec<AgentEvent>,
}

/// Obtains the provider stream (with retry + per-attempt timeout) and drains it,
/// cancellable mid-stream. Retries only before the first event is observed.
async fn stream_step(
    provider: &dyn Provider,
    request: ChatRequest,
    retry: &RetryPolicy,
    cancel: &CancelToken,
) -> StepStreamResult {
    let mut result = StepStreamResult {
        first_error: None,
        cancelled: false,
        text: String::new(),
        reasoning: String::new(),
        signature: None,
        tool_calls: Vec::new(),
        step_usage: Usage::default(),
        stop: StopReason::EndTurn,
        deltas: Vec::new(),
    };

    // Acquire the stream + first event with retry; no tokens have been emitted
    // yet at this point, so retrying is safe.
    let (first, mut stream) = match acquire_stream(provider, request, retry, cancel).await {
        AcquireOutcome::Cancelled => {
            result.cancelled = true;
            return result;
        }
        AcquireOutcome::Error(e) => {
            result.first_error = Some(e);
            return result;
        }
        AcquireOutcome::Ready { first, stream } => (first, stream),
    };

    // Process the first event, then the rest. Once we are here, do not retry.
    let mut pending = Some(first);
    loop {
        let event = match pending.take() {
            Some(ev) => ev,
            None => {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        result.cancelled = true;
                        return result;
                    }
                    next = stream.next() => match next {
                        None => break,
                        Some(Ok(ev)) => ev,
                        Some(Err(e)) => {
                            // Mid-stream error: surface it (no retry once started).
                            result.first_error = Some(e);
                            return result;
                        }
                    }
                }
            }
        };
        apply_event(&mut result, event);
    }

    result
}

/// Applies one stream event to the accumulator, recording forwardable deltas.
fn apply_event(result: &mut StepStreamResult, event: StreamEvent) {
    match event {
        StreamEvent::Start { usage } => result.step_usage = usage,
        StreamEvent::TextDelta(t) => {
            result.text.push_str(&t);
            result.deltas.push(AgentEvent::MessageDelta(t));
        }
        StreamEvent::ReasoningDelta(t) => {
            result.reasoning.push_str(&t);
            result.deltas.push(AgentEvent::ReasoningDelta(t));
        }
        StreamEvent::SignatureDelta(s) => {
            result
                .signature
                .get_or_insert_with(String::new)
                .push_str(&s);
        }
        StreamEvent::ToolCall { id, name, input } => {
            result
                .tool_calls
                .push((id.clone(), name.clone(), input.clone()));
            result.deltas.push(AgentEvent::ToolCall {
                id,
                name,
                args: input,
            });
        }
        StreamEvent::End { stop, usage } => {
            result.stop = stop;
            result.step_usage = usage;
        }
    }
}

/// The result of trying to acquire a provider stream + its first event.
enum AcquireOutcome {
    Ready {
        first: StreamEvent,
        stream: crate::provider::EventStream,
    },
    Error(crate::error::ProviderError),
    Cancelled,
}

/// Tries up to `max_retries + 1` times to open the stream and read its first
/// event within `timeout` each attempt; retries transient/rate-limited failures
/// with backoff. No tokens are emitted before this returns.
async fn acquire_stream(
    provider: &dyn Provider,
    request: ChatRequest,
    retry: &RetryPolicy,
    cancel: &CancelToken,
) -> AcquireOutcome {
    let attempts = retry.max_retries.saturating_add(1);
    let mut last_err: Option<crate::error::ProviderError> = None;

    for attempt in 0..attempts {
        if cancel.is_cancelled() {
            return AcquireOutcome::Cancelled;
        }
        if attempt > 0 {
            let backoff = retry.backoff(attempt - 1);
            tokio::select! {
                biased;
                () = cancel.cancelled() => return AcquireOutcome::Cancelled,
                () = tokio::time::sleep(backoff) => {}
            }
        }

        let req = request.clone();
        let attempt_fut = async {
            let mut stream = provider.stream(req).await?;
            let first = stream.next().await;
            Ok::<_, crate::error::ProviderError>((first, stream))
        };

        let timed = tokio::select! {
            biased;
            () = cancel.cancelled() => return AcquireOutcome::Cancelled,
            r = tokio::time::timeout(retry.timeout, attempt_fut) => r,
        };

        match timed {
            // Timed out obtaining the stream / first event → transient.
            Err(_elapsed) => {
                last_err = Some(crate::error::ProviderError::Transport(
                    "provider call timed out".into(),
                ));
            }
            Ok(Err(e)) => {
                let retryable = retry.is_retryable(&crate::retry::classify(&e));
                last_err = Some(e);
                if !retryable {
                    break;
                }
            }
            Ok(Ok((first, stream))) => match first {
                None => {
                    // Empty stream: nothing to retry on; treat as a clean,
                    // empty step by synthesizing an End.
                    return AcquireOutcome::Ready {
                        first: StreamEvent::End {
                            stop: StopReason::EndTurn,
                            usage: Usage::default(),
                        },
                        stream,
                    };
                }
                Some(Ok(ev)) => return AcquireOutcome::Ready { first: ev, stream },
                Some(Err(e)) => {
                    let retryable = retry.is_retryable(&crate::retry::classify(&e));
                    last_err = Some(e);
                    if !retryable {
                        break;
                    }
                }
            },
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

/// The built-in tool-search tool definition.
fn tool_search_def() -> ToolDef {
    ToolDef::new(
        TOOL_SEARCH,
        "Search for additional tools by keyword. Matched tools become available to call on the next step.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Keywords to match against tool names and descriptions" }
            },
            "required": ["query"]
        }),
    )
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
    use std::sync::atomic::{AtomicU32, Ordering};

    use futures::future::BoxFuture;

    use super::*;
    use crate::provider::{ChatResponse, EventStream};

    /// A provider that fails transiently `fail_times` then streams one text.
    struct FlakyProvider {
        attempts: Arc<AtomicU32>,
        fail_times: u32,
    }

    impl Provider for FlakyProvider {
        #[allow(
            clippy::unnecessary_literal_bound,
            reason = "trait method must return &str"
        )]
        fn model_id(&self) -> &str {
            "flaky"
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
            let n = self.attempts.fetch_add(1, Ordering::SeqCst);
            let fail = n < self.fail_times;
            Box::pin(async move {
                if fail {
                    return Err(crate::error::ProviderError::Transport("boom".into()));
                }
                Ok(crate::provider::event_stream(vec![
                    Ok(StreamEvent::TextDelta("ok".into())),
                    Ok(StreamEvent::End {
                        stop: StopReason::EndTurn,
                        usage: Usage::default(),
                    }),
                ]))
            })
        }
    }

    #[test]
    fn provider_registers_and_sets_default_model() {
        let agent = Agent::new(()).provider(FlakyProvider {
            attempts: Arc::new(AtomicU32::new(0)),
            fail_times: 0,
        });
        assert!(agent.has_provider("flaky"));
        assert!(!agent.has_provider("missing"));
        assert_eq!(agent.current_model(), "flaky");
    }

    #[tokio::test]
    async fn transient_failures_are_retried_before_first_token() {
        let attempts = Arc::new(AtomicU32::new(0));
        let mut agent = Agent::new(())
            .provider(FlakyProvider {
                attempts: Arc::clone(&attempts),
                // Fail twice, succeed on the third attempt (within 2 retries).
                fail_times: 2,
            })
            .model("flaky")
            .with_retries(2)
            .with_context(vec![Message::user("hi")]);
        let out = agent.run().await.expect("outcome");
        assert_eq!(out.text, "ok");
        assert!(matches!(out.finish, Finish::EndTurn));
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "two retries + success");
    }

    #[tokio::test]
    async fn transient_failures_beyond_cap_stop_with_error() {
        let attempts = Arc::new(AtomicU32::new(0));
        let mut agent = Agent::new(())
            .provider(FlakyProvider {
                attempts: Arc::clone(&attempts),
                fail_times: 5,
            })
            .model("flaky")
            .with_retries(1)
            .with_context(vec![Message::user("hi")]);
        let out = agent.run().await.expect("outcome");
        assert!(matches!(out.finish, Finish::Stopped { .. }));
        // 1 try + 1 retry = 2 attempts, then give up.
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }
}
