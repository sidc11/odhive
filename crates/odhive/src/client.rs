//! A small HTTP client for the API of the running OdHive service.
//!
//! It connects to `127.0.0.1:<port>` and signs in with the bearer token
//! (`Authorization: Bearer …`), marks every request with `X-OdHive: 1` and
//! sends no `Origin`, like any non-browser client.

use std::time::Duration;

use anyhow::{Context, bail};
use http_body_util::{BodyExt, Empty, Limited};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode, header};
use hyper_util::rt::TokioIo;
use odhive_core::token::Token;
use serde::de::DeserializeOwned;

/// Longest a request may take, connecting included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest response body read.
const MAX_BODY: usize = 4 * 1024 * 1024;

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
        if !self.status.is_success() {
            bail!("{}", self.error_text());
        }
        serde_json::from_slice(&self.body).context("unexpected answer from the OdHive service")
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
        tokio::time::timeout(REQUEST_TIMEOUT, self.send(method, path))
            .await
            .unwrap_or_else(|_| bail!("the OdHive service did not answer in time"))
    }

    async fn send(&self, method: Method, path: &str) -> anyhow::Result<Reply> {
        let stream = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, self.port))
            .await
            .with_context(|| format!("could not reach the OdHive service on port {}", self.port))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .context("could not talk to the OdHive service")?;
        let conn = tokio::spawn(conn);
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, format!("127.0.0.1:{}", self.port))
            .header(header::AUTHORIZATION, &self.authorization)
            .header("x-odhive", "1")
            .body(Empty::<Bytes>::new())
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
}
