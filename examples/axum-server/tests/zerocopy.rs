//! Proves the headline claim: parameters arrive from axum **zero-copy**.
//!
//! Two angles:
//! 1. A direct deserialize that asserts the borrowed `&str` points *into* the
//!    request buffer (if serde had copied, the pointer would be elsewhere).
//! 2. A real in-process HTTP round-trip through the axum `/login` route.

#![allow(clippy::unwrap_used)]

use axum::body::{Body, Bytes, to_bytes};
use axum::http::{Request, StatusCode};
use axum_server_example::{LoginUserParams, app};
use stakit_model::Validate as _;
use tower::ServiceExt as _; // `oneshot`

/// Zero-copy proof: the deserialized `email` borrows from the request `Bytes`.
#[test]
fn login_params_borrow_from_request_buffer() {
    // Stand-in for the buffer axum hands a handler.
    let body = Bytes::from_static(br#"{"email":"user@example.com"}"#);

    let params: LoginUserParams<'_> = serde_json::from_slice(&body).unwrap();
    params.validate().unwrap();
    assert_eq!(params.email, "user@example.com");

    // If parsing had allocated an owned String, `email` would point outside
    // `body`. Because it is `&'a str`, it points *inside* the original buffer.
    let buf = body.as_ptr() as usize;
    let buf_end = buf + body.len();
    let email = params.email.as_ptr() as usize;
    assert!(
        (buf..buf_end).contains(&email),
        "email must borrow the request buffer (zero-copy); pointed outside it"
    );
}

/// End-to-end: POST raw JSON to the real axum route; the handler deserializes
/// zero-copy, validates, and echoes the borrowed email back.
#[tokio::test]
async fn login_route_accepts_valid_email() {
    let response = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .body(Body::from(r#"{"email":"a@b.com"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["email"], "a@b.com");
}

/// The same route rejects an invalid email via the derived validation.
#[tokio::test]
async fn login_route_rejects_invalid_email() {
    let response = app()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/login")
                .body(Body::from(r#"{"email":"not-an-email"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}
