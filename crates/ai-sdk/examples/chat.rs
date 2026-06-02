//! Minimal streaming chat against Claude.
//!
//! ```bash
//! cargo run -p stakit-ai-sdk --example chat
//! ```
//! Requires `ANTHROPIC_API_KEY` (loaded from the repo-root `.env`).

use futures::StreamExt;
use stakit_ai_sdk::{Agent, CancelToken, ClaudeClient, LoopEvent, Message};

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

    let agent = Agent::<_, ()>::builder(client.model(MODEL))
        .max_tokens(256)
        .build();

    let mut stream = Box::pin(agent.run(
        vec![Message::user_text("Say hello in exactly five words.")],
        (),
        CancelToken::new(),
    ));

    while let Some(event) = stream.next().await {
        match event {
            LoopEvent::TextDelta(text) => print!("{text}"),
            LoopEvent::Done { usage, cost, .. } => {
                println!(
                    "\n\n[tokens in={} out={} | est. ${:.6}]",
                    usage.input_tokens,
                    usage.output_tokens,
                    cost.unwrap_or(0.0)
                );
            }
            _ => {}
        }
    }
}
