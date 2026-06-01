//! WebSocket / duplex transport.

use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt as _, StreamExt as _};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use stakit_router::{Endpoint, ErrorBody};

use crate::TransportError;
use crate::client::Client;
use crate::options::CallOpts;
use crate::result::ActionResult;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

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
}

impl Client {
    /// Opens a duplex websocket connection (uses the ws url, then per-call `url`,
    /// then the base url; `http(s)` is rewritten to `ws(s)`).
    ///
    /// # Errors
    /// Returns `Err` if the handshake fails.
    pub async fn connect(&self, opts: CallOpts) -> Result<Connection, TransportError> {
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

        let (socket, _response) = connect_async(request)
            .await
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
    ///
    /// # Errors
    /// Returns `Err` on a websocket-level failure.
    pub async fn recv(&mut self) -> Option<Result<ServerFrame, TransportError>> {
        loop {
            match self.stream.next().await? {
                Ok(Message::Text(text)) => return Some(parse_frame(text.as_bytes())),
                Ok(Message::Binary(bytes)) => return Some(parse_frame(bytes.as_ref())),
                Ok(Message::Close(_)) => return None,
                Ok(_) => {}
                Err(error) => return Some(Err(TransportError::WebSocket(error.to_string()))),
            }
        }
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

/// Parses a server frame from raw bytes.
fn parse_frame(bytes: &[u8]) -> Result<ServerFrame, TransportError> {
    let value: Value = serde_json::from_slice(bytes).map_err(TransportError::Decode)?;
    let id = value.get("id").and_then(Value::as_u64).unwrap_or(0);
    match value.get("kind").and_then(Value::as_str) {
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
                        message: "missing error body".to_owned(),
                        fields: None,
                    });
                ActionResult::Error(error)
            } else {
                ActionResult::Ok(value.get("data").cloned().unwrap_or(Value::Null))
            };
            Ok(ServerFrame::Result { id, result })
        }
        Some("end") => Ok(ServerFrame::End { id }),
        Some("client_call") => Ok(ServerFrame::ClientCall {
            id,
            action: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            data: value.get("params").cloned().unwrap_or(Value::Null),
        }),
        _ => Err(TransportError::WebSocket("unknown frame kind".to_owned())),
    }
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
