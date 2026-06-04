//! Minimal streaming chat against Claude.
//!
//! ```bash
//! cargo run -p stakit-ai-sdk --example chat
//! ```
//! Requires `ANTHROPIC_API_KEY` (loaded from the repo-root `.env`).

use futures::StreamExt;
use stakit_ai_sdk::{Agent, AgentEvent, ClaudeClient, Message};

const MODEL: &str = "claude-haiku-4-5-20251001";

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    let client = match ClaudeClient::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };

    let mut agent = Agent::new(())
        .provider(client.model(MODEL))
        .system("You are a helpful assistant.")
        .max_tokens(256)
        .with_context(vec![Message::user("Say hello in exactly five words.")]);

    let mut run = agent.run();
    while let Some(ev) = run.next().await {
        match ev {
            AgentEvent::MessageDelta(text) => print!("{text}"),
            AgentEvent::Done(out) => {
                println!(
                    "\n\n[tokens in={} out={} | est. ${:.6}]",
                    out.usage.input_tokens,
                    out.usage.output_tokens,
                    out.cost.unwrap_or(0.0)
                );
            }
            _ => {}
        }
    }
}
