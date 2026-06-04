//! An agent with a `#[tool]` weather function.
//!
//! ```bash
//! cargo run -p stakit-ai-sdk --example weather_agent
//! ```
//! Requires `ANTHROPIC_API_KEY` (loaded from the repo-root `.env`).

use futures::StreamExt;
use stakit_ai_sdk::{Agent, AgentEvent, ClaudeClient, Message, ToolOutcome, tool};
use stakit_model::{JsonSchema, Model};

const MODEL: &str = "claude-haiku-4-5-20251001";

#[derive(serde::Deserialize, Model, JsonSchema)]
struct WeatherArgs {
    /// City name, e.g. "Tokyo"
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

    let mut agent = Agent::new(())
        .provider(client.model(MODEL))
        .system("You are a helpful assistant. Always use tools when available.")
        .max_tokens(512)
        .register_tool(get_weather)
        .with_context(vec![Message::user(
            "What is the weather in Tokyo? Use the tool.",
        )]);

    let mut run = agent.run();
    while let Some(ev) = run.next().await {
        match ev {
            AgentEvent::MessageDelta(text) => print!("{text}"),
            AgentEvent::ToolCall { name, args, .. } => {
                println!("\n[tool call] {name}({args})");
            }
            AgentEvent::ToolResult { name, result, .. } => {
                let output = match &result {
                    ToolOutcome::Ok(v) => v.to_string(),
                    ToolOutcome::Denied { message } => format!("denied: {message}"),
                    ToolOutcome::Error(e) => format!("error: {e}"),
                };
                println!("[tool result {name}] {output}");
            }
            AgentEvent::StepEnd { usage, cost, .. } => {
                println!(
                    "\n[step] in={} out={} est=${:.6}",
                    usage.input_tokens,
                    usage.output_tokens,
                    cost.unwrap_or(0.0)
                );
            }
            AgentEvent::Done(out) => {
                println!(
                    "\n[done {:?}] total in={} out={} est=${:.6}",
                    out.finish,
                    out.usage.input_tokens,
                    out.usage.output_tokens,
                    out.cost.unwrap_or(0.0)
                );
            }
            _ => {}
        }
    }
}
