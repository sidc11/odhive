//! Authentication of the API and WebSocket.
//!
//! A request is let in when it carries the token (see
//! [`odhive_core::token`]) either in the session cookie (web UI) or as
//! `Authorization: Bearer <token>` (CLI). The browser gets the cookie by
//! opening `/login?t=<code>` with a one-time code: valid for 60 s, used
//! once, issued only to clients that already hold the token. The cookie is
//! `HttpOnly` and `SameSite=Strict`; `/login` then redirects to `/` so the
//! code leaves the address bar.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use odhive_core::token::{self, Token, TokenError};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::api::error::{ApiError, ApiResult, ErrorBody};
use crate::service::Service;

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "odhive_session";

/// Where the browser trades a one-time code for the session cookie.
pub const LOGIN_PATH: &str = "/login";

/// How long a one-time login code stays valid.
pub const CODE_TTL: Duration = Duration::from_secs(60);

/// How long the browser keeps the session cookie.
pub const SESSION_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Most unused codes kept at once; issuing more drops the oldest.
const MAX_CODES: usize = 32;

/// Bytes of randomness in a login code.
const CODE_BYTES: usize = 32;

/// The token plus the outstanding login codes. Cheap to clone.
#[derive(Clone)]
pub struct Auth {
    inner: Arc<Inner>,
}

struct Inner {
    token: Token,
    ttl: Duration,
    /// Unused codes (hex) with the moment they expire, oldest first.
    codes: Mutex<Vec<(String, Instant)>>,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth").finish_non_exhaustive()
    }
}

/// A one-time login code.
#[derive(Debug, Clone, Serialize, serde::Deserialize, ToSchema)]
pub struct LoginCode {
    /// The code (64 hex characters).
    pub code: String,
    /// Path to open in the browser: `/login?t=<code>`.
    #[schema(example = "/login?t=0123…")]
    pub path: String,
    /// Seconds until the code expires.
    #[schema(example = 60)]
    pub expires_in: u64,
}

impl Auth {
    /// Authentication with `token` and codes valid for [`CODE_TTL`].
    #[must_use]
    pub fn new(token: Token) -> Self {
        Self::with_code_ttl(token, CODE_TTL)
    }

    /// Like [`Auth::new`] with codes valid for `ttl`.
    #[must_use]
    pub fn with_code_ttl(token: Token, ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                token,
                ttl,
                codes: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Issues a new one-time login code.
    ///
    /// # Errors
    /// When no secure random numbers are available.
    pub fn issue_code(&self) -> Result<LoginCode, TokenError> {
        let mut raw = [0u8; CODE_BYTES];
        token::fill_random(&mut raw)?;
        let code = token::to_hex(&raw);
        let now = Instant::now();
        let mut codes = self.codes();
        codes.retain(|(_, expires)| *expires > now);
        if codes.len() >= MAX_CODES {
            codes.remove(0);
        }
        codes.push((code.clone(), now + self.inner.ttl));
        Ok(LoginCode {
            path: format!("{LOGIN_PATH}?t={code}"),
            code,
            expires_in: self.inner.ttl.as_secs(),
        })
    }

    /// Uses up `code`: true if it was issued, unused and not expired.
    #[must_use]
    pub fn redeem(&self, code: &str) -> bool {
        let now = Instant::now();
        let mut codes = self.codes();
        codes.retain(|(_, expires)| *expires > now);
        // Compare against every code so timing says nothing about which matched.
        let mut found = None;
        for (i, (known, _)) in codes.iter().enumerate() {
            if token::constant_time_eq(known.as_bytes(), code.as_bytes()) {
                found = Some(i);
            }
        }
        found.map(|i| codes.remove(i)).is_some()
    }

    /// Whether `headers` carry the token, in the session cookie or as a
    /// bearer token.
    #[must_use]
    pub fn is_authorized(&self, headers: &HeaderMap) -> bool {
        let bearer = headers
            .get_all(header::AUTHORIZATION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(bearer_token);
        let cookies = headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|pair| pair.trim().split_once('='))
            .filter(|(name, _)| *name == SESSION_COOKIE)
            .map(|(_, value)| value);
        let mut ok = false;
        for candidate in bearer.chain(cookies) {
            ok |= self.inner.token.matches(candidate);
        }
        ok
    }

    /// The `Set-Cookie` value that logs a browser in.
    fn session_cookie(&self) -> String {
        format!(
            "{SESSION_COOKIE}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
            self.inner.token.to_hex(),
            SESSION_MAX_AGE.as_secs()
        )
    }

    fn codes(&self) -> std::sync::MutexGuard<'_, Vec<(String, Instant)>> {
        self.inner
            .codes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The token of an `Authorization: Bearer <token>` value.
fn bearer_token(value: &str) -> Option<&str> {
    let (scheme, rest) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| rest.trim_matches(' '))
}

/// Rejects requests that carry no valid token with `401 unauthorized`.
pub async fn require_auth(State(auth): State<Auth>, req: Request, next: Next) -> Response {
    if auth.is_authorized(req.headers()) {
        return next.run(req).await;
    }
    let mut resp = ApiError::new(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "not signed in: run `odhive` to open a sign-in link",
    )
    .into_response();
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"odhive\""),
    );
    resp
}

/// Query of [`login`].
#[derive(Debug, Deserialize)]
pub(crate) struct LoginQuery {
    t: Option<String>,
}

const LOGIN_FAILED: &str = "This OdHive sign-in link is invalid, expired or was already used.\n\
Run `odhive` in a terminal to get a new one.\n";

/// `GET /login?t=<code>`: trades a one-time code for the session cookie and
/// redirects to `/`. A browser that is already signed in is just redirected.
pub(crate) async fn login(
    State(auth): State<Auth>,
    headers: HeaderMap,
    query: Result<Query<LoginQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let code = query.ok().and_then(|Query(q)| q.t);
    let redeemed = code.as_deref().is_some_and(|c| auth.redeem(c));
    let mut resp = if redeemed {
        let mut resp = redirect_home();
        if let Ok(cookie) = HeaderValue::from_str(&auth.session_cookie()) {
            resp.headers_mut().insert(header::SET_COOKIE, cookie);
        }
        resp
    } else if auth.is_authorized(&headers) {
        redirect_home()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            LOGIN_FAILED,
        )
            .into_response()
    };
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn redirect_home() -> Response {
    (StatusCode::SEE_OTHER, [(header::LOCATION, "/")]).into_response()
}

/// Issues a one-time code that signs a browser in when it opens
/// `path` (valid for 60 s, once).
#[utoipa::path(operation_id = "create_login_code", post, path = "/auth/codes", tag = "service",
    responses(
        (status = CREATED, description = "A new code", body = LoginCode),
        (status = INTERNAL_SERVER_ERROR, description = "No random numbers", body = ErrorBody),
    ))]
pub(crate) async fn create_code(
    State(svc): State<Service>,
) -> ApiResult<(StatusCode, Json<LoginCode>)> {
    let code = svc
        .auth()
        .issue_code()
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok((StatusCode::CREATED, Json(code)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth() -> (Auth, String) {
        let token = Token::generate().expect("token");
        let hex = token.to_hex();
        (Auth::new(token), hex)
    }

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.append(name.clone(), HeaderValue::from_str(value).expect("value"));
        }
        h
    }

    #[test]
    fn codes_work_once() {
        let (auth, _) = auth();
        let a = auth.issue_code().expect("a");
        let b = auth.issue_code().expect("b");
        assert_ne!(a.code, b.code);
        assert_eq!(a.code.len(), 64);
        assert_eq!(a.path, format!("/login?t={}", a.code));
        assert_eq!(a.expires_in, 60);
        assert!(auth.redeem(&b.code));
        assert!(!auth.redeem(&b.code), "reused");
        assert!(auth.redeem(&a.code));
        assert!(!auth.redeem(""));
        assert!(!auth.redeem("nope"));
    }

    #[test]
    fn codes_expire() {
        let token = Token::generate().expect("token");
        let auth = Auth::with_code_ttl(token, Duration::from_millis(30));
        let code = auth.issue_code().expect("code");
        std::thread::sleep(Duration::from_millis(60));
        assert!(!auth.redeem(&code.code));
    }

    #[test]
    fn too_many_codes_drop_the_oldest() {
        let (auth, _) = auth();
        let first = auth.issue_code().expect("first");
        let rest: Vec<_> = (0..MAX_CODES)
            .map(|_| auth.issue_code().expect("code"))
            .collect();
        assert!(!auth.redeem(&first.code));
        assert!(rest.iter().all(|c| auth.redeem(&c.code)));
    }

    #[test]
    fn cookie_or_bearer_authorizes() {
        let (auth, hex) = auth();
        let cookie = format!("theme=dark; {SESSION_COOKIE}={hex}; x=1");
        assert!(auth.is_authorized(&headers(&[(header::COOKIE, &cookie)])));
        let split = [
            (header::COOKIE, "theme=dark"),
            (header::COOKIE, &*format!("{SESSION_COOKIE}={hex}")),
        ];
        assert!(auth.is_authorized(&headers(&split)));
        for value in [format!("Bearer {hex}"), format!("bearer  {hex}")] {
            assert!(
                auth.is_authorized(&headers(&[(header::AUTHORIZATION, &value)])),
                "{value}"
            );
        }
    }

    #[test]
    fn anything_else_is_refused() {
        let (auth, hex) = auth();
        let other = Token::generate().expect("other").to_hex();
        let refused = [
            vec![],
            vec![(header::COOKIE, format!("{SESSION_COOKIE}={other}"))],
            vec![(header::COOKIE, format!("{SESSION_COOKIE}={}", &hex[..63]))],
            vec![(header::COOKIE, format!("{SESSION_COOKIE}= {hex}x"))],
            vec![(header::COOKIE, format!("other_session={hex}"))],
            vec![(header::COOKIE, format!("x{SESSION_COOKIE}={hex}"))],
            vec![(header::AUTHORIZATION, hex.clone())],
            vec![(header::AUTHORIZATION, format!("Basic {hex}"))],
            vec![(header::AUTHORIZATION, format!("Bearer {other}"))],
            vec![(header::AUTHORIZATION, "Bearer".to_owned())],
            vec![(
                header::AUTHORIZATION,
                format!("Bearer {}", hex.to_uppercase()),
            )],
        ];
        for pairs in refused {
            let pairs: Vec<_> = pairs.iter().map(|(n, v)| (n.clone(), v.as_str())).collect();
            assert!(!auth.is_authorized(&headers(&pairs)), "{pairs:?}");
        }
    }

    #[test]
    fn session_cookie_is_http_only_and_strict() {
        let (auth, hex) = auth();
        let cookie = auth.session_cookie();
        assert!(cookie.starts_with(&format!("{SESSION_COOKIE}={hex};")));
        assert!(cookie.contains("; HttpOnly"));
        assert!(cookie.contains("; SameSite=Strict"));
        assert!(cookie.contains("; Path=/"));
        assert!(!cookie.contains("Domain"));
        assert_eq!(format!("{auth:?}"), "Auth { .. }");
    }
}
