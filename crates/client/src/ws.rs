//! WebSocket / duplex transport.

use std::sync::{Arc, Once};
use std::time::Duration;

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt as _, StreamExt as _};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use stakit_router::{Endpoint, ErrorBody};

use crate::TransportError;
use crate::client::Client;
use crate::options::CallOpts;
use crate::result::ActionResult;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long the `wss`/`ws` handshake may take before giving up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Installs the process-wide rustls crypto provider once, so `wss://` works.
/// Matches reqwest's backend (aws-lc-rs), so a single provider serves both; the
/// call is a no-op if something already installed one.
fn ensure_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// A live duplex websocket connection.
///
/// Drive it explicitly: [`Connection::send`] to invoke server actions,
/// [`Connection::recv`] to pull frames. When a frame is
/// [`ServerFrame::ClientCall`], run your handler and answer with
/// [`Connection::reply`] / [`Connection::reply_error`].
pub struct Connection {
    sink: SplitSink<Ws, Message>,
    stream: SplitStream<Ws>,
    next_id: u64,
}

/// A frame received from the server over a [`Connection`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ServerFrame {
    /// A result for a call (matched to its `id`). `data` is left untyped because
    /// one connection multiplexes many actions; deserialize it yourself.
    Result {
        /// The call id this answers.
        id: u64,
        /// The (untyped) outcome.
        result: ActionResult<Value>,
    },
    /// A stream action finished.
    End {
        /// The call id that ended.
        id: u64,
    },
    /// The server is invoking a client action; answer with [`Connection::reply`].
    ClientCall {
        /// The call id to answer with.
        id: u64,
        /// The client action name.
        action: String,
        /// The (untyped) params.
        data: Value,
    },
    /// A pub/sub message on a subscribed topic. A `seq` that isn't the last
    /// one + 1, or a new `epoch`, means messages were missed: resync.
    Publish {
        /// The topic it was published on.
        topic: String,
        /// The client action name.
        name: String,
        /// Per-topic sequence number.
        seq: u64,
        /// Topic history epoch.
        epoch: u64,
        /// The (untyped) params.
        params: Value,
    },
    /// The topic's history reset (server lost messages): resync, then continue
    /// from `epoch` / `seq`.
    Reset {
        /// The topic.
        topic: String,
        /// The new epoch.
        epoch: u64,
        /// The new sequence number.
        seq: u64,
    },
    /// The server removed this subscription (e.g. membership revoked).
    Unsubscribed {
        /// The topic.
        topic: String,
    },
}

impl Client {
    /// Opens a duplex websocket connection (uses the ws url, then per-call `url`,
    /// then the base url; `http(s)` is rewritten to `ws(s)`).
    ///
    /// # Errors
    /// Returns `Err` if the handshake fails.
    pub async fn connect(&self, opts: CallOpts) -> Result<Connection, TransportError> {
        ensure_crypto_provider();
        let url = opts
            .url
            .as_deref()
            .or_else(|| self.ws_url())
            .unwrap_or_else(|| self.base_url());
        let url = to_ws_url(url);

        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| TransportError::WebSocket(e.to_string()))?;
        for (name, value) in self.merged_headers(&opts.headers) {
            if let (Ok(header_name), Ok(header_value)) = (
                name.parse::<tokio_tungstenite::tungstenite::http::HeaderName>(),
                HeaderValue::from_str(&value),
            ) {
                request.headers_mut().insert(header_name, header_value);
            }
        }

        // `None` connector → tokio-tungstenite builds the default rustls (webpki
        // roots) connector for `wss://`; `Some` overrides it to skip verification.
        let connector = if self.accept_invalid_certs() {
            Some(Connector::Rustls(Arc::new(no_verify_config())))
        } else {
            None
        };

        // Bound the handshake (TLS + upgrade) so a black-holed host can't hang us.
        let connecting = connect_async_tls_with_config(request, None, false, connector);
        let (socket, _response) = tokio::time::timeout(HANDSHAKE_TIMEOUT, connecting)
            .await
            .map_err(|_| TransportError::WebSocket("websocket handshake timed out".to_owned()))?
            .map_err(|e| TransportError::WebSocket(e.to_string()))?;
        let (sink, stream) = socket.split();
        Ok(Connection {
            sink,
            stream,
            next_id: 1,
        })
    }
}

impl Connection {
    /// Invokes a server action, returning the call id its results are tagged with.
    ///
    /// # Errors
    /// Returns `Err` if encoding or sending fails.
    pub async fn send<E>(&mut self, _endpoint: E, params: E::Params) -> Result<u64, TransportError>
    where
        E: Endpoint,
        E::Params: Serialize,
    {
        let id = self.next_id;
        self.next_id += 1;
        let params = serde_json::to_value(&params).map_err(TransportError::Encode)?;
        let frame = json!({ "kind": "call", "id": id, "action": E::ACTION, "params": params });
        self.send_value(&frame).await?;
        Ok(id)
    }

    /// Authenticates the session (`auth` frame); the answer is a
    /// [`ServerFrame::Result`] tagged with the returned id.
    ///
    /// # Errors
    /// Returns `Err` if encoding or sending fails.
    pub async fn auth<T: Serialize>(&mut self, params: T) -> Result<u64, TransportError> {
        let params = serde_json::to_value(&params).map_err(TransportError::Encode)?;
        self.send_frame("auth", json!({ "params": params })).await
    }

    /// Subscribes to `channel` with `key`; the [`ServerFrame::Result`] for the
    /// returned id carries `{ topic, epoch, seq }` (or the denial).
    ///
    /// # Errors
    /// Returns `Err` if encoding or sending fails.
    pub async fn subscribe<K: Serialize>(
        &mut self,
        channel: &str,
        key: K,
    ) -> Result<u64, TransportError> {
        let key = serde_json::to_value(&key).map_err(TransportError::Encode)?;
        self.send_frame("subscribe", json!({ "channel": channel, "key": key }))
            .await
    }

    /// Unsubscribes from `topic` (as returned by the subscribe result).
    ///
    /// # Errors
    /// Returns `Err` if sending fails.
    pub async fn unsubscribe(&mut self, topic: &str) -> Result<u64, TransportError> {
        self.send_frame("unsubscribe", json!({ "topic": topic }))
            .await
    }

    /// Sends a `{ kind, id, ...fields }` request frame with a fresh id.
    async fn send_frame(&mut self, kind: &str, mut fields: Value) -> Result<u64, TransportError> {
        let id = self.next_id;
        self.next_id += 1;
        if let Some(map) = fields.as_object_mut() {
            map.insert("kind".to_owned(), json!(kind));
            map.insert("id".to_owned(), json!(id));
        }
        self.send_value(&fields).await?;
        Ok(id)
    }

    /// Answers a [`ServerFrame::ClientCall`] with success data.
    ///
    /// # Errors
    /// Returns `Err` if encoding or sending fails.
    pub async fn reply<T: Serialize>(&mut self, id: u64, data: T) -> Result<(), TransportError> {
        let data = serde_json::to_value(&data).map_err(TransportError::Encode)?;
        self.send_value(&json!({ "kind": "client_result", "id": id, "data": data }))
            .await
    }

    /// Answers a [`ServerFrame::ClientCall`] with an error.
    ///
    /// # Errors
    /// Returns `Err` if sending fails.
    pub async fn reply_error(&mut self, id: u64, error: ErrorBody) -> Result<(), TransportError> {
        let error = serde_json::to_value(&error).map_err(TransportError::Encode)?;
        self.send_value(&json!({ "kind": "client_result", "id": id, "error": error }))
            .await
    }

    /// Reads the next frame from the server, or `None` when the socket closes.
    /// Server heartbeat `ping` frames are answered (`pong`) transparently.
    ///
    /// # Errors
    /// Returns `Err` on a websocket-level failure.
    pub async fn recv(&mut self) -> Option<Result<ServerFrame, TransportError>> {
        loop {
            let frame = match self.stream.next().await? {
                Ok(Message::Text(text)) => parse_frame(text.as_bytes()),
                Ok(Message::Binary(bytes)) => parse_frame(bytes.as_ref()),
                // Keepalive: answer server pings (the split sink isn't auto-ponged).
                Ok(Message::Ping(payload)) => {
                    let _ = self.sink.send(Message::Pong(payload)).await;
                    continue;
                }
                Ok(Message::Pong(_) | Message::Frame(_)) => continue,
                Ok(Message::Close(_)) => return None,
                Err(error) => return Some(Err(TransportError::WebSocket(error.to_string()))),
            };
            match frame {
                Ok(None) => {
                    if let Err(error) = self.send_value(&json!({ "kind": "pong" })).await {
                        return Some(Err(error));
                    }
                }
                Ok(Some(frame)) => return Some(Ok(frame)),
                Err(error) => return Some(Err(error)),
            }
        }
    }

    /// Sends a ping (client-initiated keepalive / liveness probe).
    ///
    /// # Errors
    /// Returns `Err` if the ping cannot be sent.
    pub async fn ping(&mut self) -> Result<(), TransportError> {
        self.sink
            .send(Message::Ping(Vec::new().into()))
            .await
            .map_err(|e| TransportError::WebSocket(e.to_string()))
    }

    /// Closes the connection.
    ///
    /// # Errors
    /// Returns `Err` if the close frame cannot be sent.
    pub async fn close(mut self) -> Result<(), TransportError> {
        self.sink
            .close()
            .await
            .map_err(|e| TransportError::WebSocket(e.to_string()))
    }

    async fn send_value(&mut self, value: &Value) -> Result<(), TransportError> {
        let text = serde_json::to_string(value).map_err(TransportError::Encode)?;
        self.sink
            .send(Message::Text(text.into()))
            .await
            .map_err(|e| TransportError::WebSocket(e.to_string()))
    }
}

fn text_field(value: &Value, name: &str) -> String {
    value
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn number_field(value: &Value, name: &str) -> u64 {
    value.get(name).and_then(Value::as_u64).unwrap_or(0)
}

/// Parses a server frame from raw bytes; `None` for a heartbeat `ping`.
fn parse_frame(bytes: &[u8]) -> Result<Option<ServerFrame>, TransportError> {
    let value: Value = serde_json::from_slice(bytes).map_err(TransportError::Decode)?;
    let id = number_field(&value, "id");
    let frame = match value.get("kind").and_then(Value::as_str) {
        Some("ping") => return Ok(None),
        Some("publish") => ServerFrame::Publish {
            topic: text_field(&value, "topic"),
            name: text_field(&value, "name"),
            seq: number_field(&value, "seq"),
            epoch: number_field(&value, "epoch"),
            params: value.get("params").cloned().unwrap_or(Value::Null),
        },
        Some("reset") => ServerFrame::Reset {
            topic: text_field(&value, "topic"),
            epoch: number_field(&value, "epoch"),
            seq: number_field(&value, "seq"),
        },
        Some("unsubscribed") => ServerFrame::Unsubscribed {
            topic: text_field(&value, "topic"),
        },
        Some("result") => {
            let result = if value.get("status").and_then(Value::as_str) == Some("error") {
                let error: ErrorBody = value
                    .get("error")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(TransportError::Decode)?
                    .unwrap_or_else(|| ErrorBody {
                        code: 500,
                        kind: std::borrow::Cow::Borrowed("internal"),
                        message: "missing error body".to_owned(),
                        fields: None,
                        data: None,
                    });
                ActionResult::Error(error)
            } else {
                ActionResult::Ok(value.get("data").cloned().unwrap_or(Value::Null))
            };
            ServerFrame::Result { id, result }
        }
        Some("end") => ServerFrame::End { id },
        Some("client_call") => ServerFrame::ClientCall {
            id,
            action: text_field(&value, "name"),
            data: value.get("params").cloned().unwrap_or(Value::Null),
        },
        _ => return Err(TransportError::WebSocket("unknown frame kind".to_owned())),
    };
    Ok(Some(frame))
}

/// Rewrites an `http(s)` url to `ws(s)`; leaves `ws(s)` untouched.
fn to_ws_url(url: &str) -> String {
    for (scheme, ws_scheme) in [("https://", "wss://"), ("http://", "ws://")] {
        if let Some(rest) = url.strip_prefix(scheme) {
            return format!("{ws_scheme}{rest}");
        }
    }
    url.to_owned()
}

/// Builds a rustls client config that skips certificate verification (used only
/// when [`Builder::danger_accept_invalid_certs`](crate::Builder::danger_accept_invalid_certs)
/// is set — trusted internal networks).
fn no_verify_config() -> rustls::ClientConfig {
    rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoCertVerification))
        .with_no_client_auth()
}

/// A `ServerCertVerifier` that accepts any certificate. Deliberately insecure;
/// gated behind the explicit `danger_accept_invalid_certs` opt-in.
#[derive(Debug)]
struct NoCertVerification;

impl ServerCertVerifier for NoCertVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
