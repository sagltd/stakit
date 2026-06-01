//! A full agent: a `#[tool]`, the `.agents/skills` skill loader, a permission
//! guard, and live per-step usage/cost — driven as an event stream.
//!
//! ```bash
//! cargo run -p stakit-ai-sdk --example weather_agent
//! ```
//! Requires `ANTHROPIC_API_KEY` (loaded from the repo-root `.env`). Run from the
//! workspace root so `.agents/skills` resolves.

use futures::StreamExt;
use stakit_ai_sdk::{
    Agent, CancelToken, ClaudeClient, FsSkillLoader, LoopEvent, Message, Permission, tool,
};
use stakit_model::{JsonSchema, Model};

const MODEL: &str = "claude-haiku-4-5-20251001";

#[derive(serde::Deserialize, Model, JsonSchema)]
struct WeatherArgs {
    /// City name, e.g. "Tokyo"
    #[validate(min_len = 1)]
    city: String,
}

/// Get the current weather for a city.
#[tool]
async fn get_weather(args: WeatherArgs) -> Result<String, ToolError> {
    Ok(format!("It is 21°C and sunny in {}.", args.city))
}

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let Ok(client) = ClaudeClient::from_env() else {
        eprintln!("set ANTHROPIC_API_KEY (see .env)");
        return;
    };

    let agent = Agent::<_, ()>::builder(client.model(MODEL))
        .model(MODEL)
        .max_tokens(512)
        .register(get_weather)
        .skills(FsSkillLoader::new(".agents/skills"))
        .can_use_tool(|name, args, _cx| {
            let name = name.to_owned();
            let args = args.clone();
            Box::pin(async move {
                println!("\n[permission] allow `{name}` with {args}");
                Permission::Allow
            })
        })
        .build();

    let mut stream = Box::pin(agent.run(
        vec![Message::user_text(
            "What is the weather in Tokyo? Use the tool.",
        )],
        (),
        CancelToken::new(),
    ));

    while let Some(event) = stream.next().await {
        match event {
            LoopEvent::TextDelta(text) => print!("{text}"),
            LoopEvent::ToolCall { name, input, .. } => println!("\n[tool call] {name}({input})"),
            LoopEvent::ToolResult { output, .. } => println!("[tool result] {output}"),
            LoopEvent::Usage { step, usage, cost } => println!(
                "[step {step}] in={} out={} est=${:.6}",
                usage.input_tokens,
                usage.output_tokens,
                cost.unwrap_or(0.0)
            ),
            LoopEvent::Done {
                reason,
                usage,
                cost,
                ..
            } => println!(
                "\n[done {reason:?}] total in={} out={} est=${:.6}",
                usage.input_tokens,
                usage.output_tokens,
                cost.unwrap_or(0.0)
            ),
            _ => {}
        }
    }
}
