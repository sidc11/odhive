//! A small HTTP client for the API of the running OdHive service.
//!
//! It connects to `127.0.0.1:<port>` and signs in with the bearer token
//! (`Authorization: Bearer …`), marks every request with `X-OdHive: 1` and
//! sends no `Origin`, like any non-browser client.

use std::time::Duration;

use anyhow::{Context, bail};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode, header};
use hyper_util::rt::TokioIo;
use odhive_core::token::Token;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;

/// Longest a request may take, connecting included.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// For requests that wait for work: starting or stopping an instance. The
/// service answers within its own 60 s request limit.
pub const SLOW_TIMEOUT: Duration = Duration::from_secs(70);

/// Largest response body read.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// Where the API lives.
pub const API: &str = odhive_server::api::PREFIX;

/// A client of the service on `port`.
#[derive(Debug, Clone)]
pub struct Client {
    port: u16,
    authorization: String,
}

/// Status and body of a response.
#[derive(Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub body: Bytes,
}

impl Reply {
    /// The body as JSON, or an error naming the API's error message.
    pub fn json<T: DeserializeOwned>(&self) -> anyhow::Result<T> {
        self.check()?;
        serde_json::from_slice(&self.body).context("unexpected answer from the OdHive service")
    }

    /// Fails with the API's error message unless the request succeeded.
    pub fn check(&self) -> anyhow::Result<()> {
        if !self.status.is_success() {
            bail!("{}", self.error_text());
        }
        Ok(())
    }

    /// The API's error `code`, if the reply is an error.
    pub fn error_code(&self) -> Option<String> {
        if self.status.is_success() {
            return None;
        }
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_owned))
    }

    /// The API's `message` (or the status) of an error reply.
    pub fn error_text(&self) -> String {
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("the OdHive service answered {}", self.status))
    }
}

impl Client {
    pub fn new(port: u16, token: &Token) -> Self {
        Self {
            port,
            authorization: format!("Bearer {}", token.to_hex()),
        }
    }

    pub async fn get(&self, path: &str) -> anyhow::Result<Reply> {
        self.request(Method::GET, path).await
    }

    pub async fn post(&self, path: &str) -> anyhow::Result<Reply> {
        self.request(Method::POST, path).await
    }

    /// Sends a request without a body.
    pub async fn request(&self, method: Method, path: &str) -> anyhow::Result<Reply> {
        self.call(method, path, None::<&()>, REQUEST_TIMEOUT).await
    }

    /// Sends a request, with `body` as JSON when given, and waits at most
    /// `timeout` for the answer.
    pub async fn call<B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
        timeout: Duration,
    ) -> anyhow::Result<Reply> {
        let body = body
            .map(serde_json::to_vec)
            .transpose()
            .context("could not encode the request")?;
        tokio::time::timeout(timeout, self.send(method, path, body))
            .await
            .unwrap_or_else(|_| bail!("the OdHive service did not answer in time"))
    }

    async fn connect(&self) -> anyhow::Result<TcpStream> {
        TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, self.port))
            .await
            .with_context(|| format!("could not reach the OdHive service on port {}", self.port))
    }

    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> anyhow::Result<Reply> {
        let stream = self.connect().await?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .context("could not talk to the OdHive service")?;
        let conn = tokio::spawn(conn);
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, self.host())
            .header(header::AUTHORIZATION, &self.authorization)
            .header("x-odhive", "1");
        if body.is_some() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        let req = req
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .context("bad request")?;
        let resp = sender
            .send_request(req)
            .await
            .context("the OdHive service closed the connection")?;
        let status = resp.status();
        let body = Limited::new(resp.into_body(), MAX_BODY)
            .collect()
            .await
            .map_err(|e| anyhow::anyhow!("could not read the answer: {e}"))?
            .to_bytes();
        drop(sender);
        conn.abort();
        Ok(Reply { status, body })
    }

    /// Opens the API's WebSocket.
    pub async fn websocket(&self) -> anyhow::Result<WebSocketStream<TcpStream>> {
        let stream = self.connect().await?;
        let req = Request::builder()
            .uri(format!("ws://{}{API}/ws", self.host()))
            .header(header::HOST, self.host())
            .header(header::AUTHORIZATION, &self.authorization)
            .header(header::CONNECTION, "Upgrade")
            .header(header::UPGRADE, "websocket")
            .header(header::SEC_WEBSOCKET_VERSION, "13")
            .header(
                header::SEC_WEBSOCKET_KEY,
                tokio_tungstenite::tungstenite::handshake::client::generate_key(),
            )
            .body(())
            .context("bad request")?;
        let (ws, _) = tokio::time::timeout(
            REQUEST_TIMEOUT,
            tokio_tungstenite::client_async(req, stream),
        )
        .await
        .map_err(|_| anyhow::anyhow!("the OdHive service did not answer in time"))?
        .context("could not open the OdHive event stream")?;
        Ok(ws)
    }
}
