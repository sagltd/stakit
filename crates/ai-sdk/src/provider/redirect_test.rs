//! Provider attempts never follow a redirect with private request material.

mod fixture;

use crate::{ChatRequest, ClaudeClient, Message, OpenAiClient, Provider};
use fixture::{BODY, Fixture, KEY};

#[tokio::test]
async fn default_client_positive_control_forwards_post_body_and_custom_key() {
    for status in [307, 308] {
        let fixture = Fixture::start(status);
        let result = reqwest::Client::builder()
            .retry(reqwest::retry::never())
            .no_proxy()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .expect("control client")
            .post(&fixture.url)
            .header("x-api-key", KEY)
            .body(BODY)
            .send()
            .await;
        let (origin, target) = fixture.finish();
        assert!(result.is_ok(), "positive control response");
        assert_eq!(origin.requests, 1);
        assert_eq!(
            target.requests, 1,
            "default redirect must reach other origin"
        );
        assert!(target.has_key, "default forwards custom credential header");
        assert!(target.has_body, "default forwards private request body");
        assert!(target.is_post, "307/308 preserves POST");
    }
}

async fn rejected(status: u16, claude: bool) {
    // Exercise both complete and stream acquisition through real adapters.
    for stream in [false, true] {
        let fixture = Fixture::start(status);
        let provider: Box<dyn Provider> = if claude {
            Box::new(
                ClaudeClient::new(KEY)
                    .with_base_url(&fixture.url)
                    .model("claude-sonnet-5-5"),
            )
        } else {
            Box::new(
                OpenAiClient::new(KEY)
                    .with_base_url(&fixture.url)
                    .model("gpt-6-sol"),
            )
        };
        let mut request = ChatRequest::new(provider.model_id());
        request.messages = vec![Message::user(BODY)];
        let failed = if stream {
            provider.stream(request).await.is_err()
        } else {
            provider.complete(request).await.is_err()
        };
        let (origin, target) = fixture.finish();
        assert_eq!(origin.requests, 1);
        assert_eq!(target.requests, 0, "provider must not forward any request");
        assert!(!target.has_key);
        assert!(!target.has_body);
        assert!(failed, "redirect must be an error, not provider success");
    }
}

#[tokio::test]
async fn claude_307_never_forwards_to_another_origin() {
    rejected(307, true).await;
}

#[tokio::test]
async fn claude_308_never_forwards_to_another_origin() {
    rejected(308, true).await;
}

#[tokio::test]
async fn openai_307_never_forwards_to_another_origin() {
    rejected(307, false).await;
}

#[tokio::test]
async fn openai_308_never_forwards_to_another_origin() {
    rejected(308, false).await;
}
