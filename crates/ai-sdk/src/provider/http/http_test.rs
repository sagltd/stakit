use serde_json::json;

use super::api_error;

#[test]
fn api_error_does_not_surface_raw_body_secrets() {
    // The raw body echoes back request material (here a fake key); only the
    // provider's structured error.message must reach Display.
    let body = json!({
        "error": { "type": "invalid_request_error", "message": "model not found" },
        "request_echo": { "x-api-key": "sk-secret-LEAK", "authorization": "Bearer sk-secret-LEAK" }
    })
    .to_string();
    let surfaced = api_error(400, &body, None).to_string();
    assert!(
        !surfaced.contains("sk-secret-LEAK"),
        "raw body leaked into Display: {surfaced}"
    );
    assert!(surfaced.contains("model not found"), "{surfaced}");
}

#[test]
fn api_error_falls_back_to_safe_message_for_opaque_body() {
    // An HTML/plain body (no JSON error.message) must not be surfaced verbatim.
    for body in [
        "<html>sk-secret-LEAK gateway</html>",
        "sk-secret-LEAK upstream timeout",
    ] {
        let surfaced = api_error(502, body, None).to_string();
        assert!(!surfaced.contains("sk-secret-LEAK"), "{surfaced}");
        assert!(surfaced.contains("502"), "{surfaced}");
    }
}

/// The client and the model handle embedding it must both redact the key.
fn assert_redacted(client: &impl std::fmt::Debug, model: &impl std::fmt::Debug) {
    let dbg = format!("{client:?}");
    assert!(!dbg.contains("sk-secret-LEAK"), "{dbg}");
    assert!(dbg.contains("[redacted]"), "{dbg}");
    let dbg = format!("{model:?}");
    assert!(!dbg.contains("sk-secret-LEAK"), "{dbg}");
}

#[cfg(feature = "claude")]
#[test]
fn claude_client_debug_redacts_api_key() {
    let client = crate::ClaudeClient::new("sk-secret-LEAK");
    assert_redacted(&client, &client.model("claude-opus-4-8"));
}

#[cfg(feature = "openai")]
#[test]
fn openai_client_debug_redacts_api_key() {
    let client = crate::OpenAiClient::new("sk-secret-LEAK");
    assert_redacted(&client, &client.model("gpt-4o"));
}
