//! Shared HTTP-client glue for thin-client CLI subcommands.
//!
//! Every state-touching subcommand (status, search, bootstrap, …) goes
//! through these helpers so URL resolution + bearer-auth handling stays
//! consistent in one place.
//!
//! ## Configuration
//!
//! Two environment variables drive the client:
//!
//! - `AI_MEMORY_SERVER_URL` — base URL. Defaults to
//!   `http://127.0.0.1:49374` for the single-laptop case.
//! - `AI_MEMORY_AUTH_TOKEN` — bearer token. Optional; only sent when
//!   non-empty. A loopback server with no token set accepts every
//!   request, so the default flow needs no credentials at all.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Resolved server target — base URL + optional bearer token.
#[derive(Debug, Clone)]
pub struct ServerEndpoint {
    /// Base URL with any trailing slash stripped, e.g.
    /// `http://127.0.0.1:49374` or `http://192.168.0.90:49374`.
    pub url: String,
    /// Bearer token when present, else `None`.
    pub auth_token: Option<String>,
}

impl ServerEndpoint {
    /// Read the endpoint from `AI_MEMORY_SERVER_URL` +
    /// `AI_MEMORY_AUTH_TOKEN`. Both are optional; the URL defaults to
    /// loopback, the token defaults to unset.
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_pair(
            std::env::var("AI_MEMORY_SERVER_URL").ok(),
            std::env::var("AI_MEMORY_AUTH_TOKEN").ok(),
        )
    }

    /// Build from an explicit URL + token pair (useful for tests that
    /// cannot safely mutate the process environment).
    ///
    /// `url` defaults to `http://127.0.0.1:49374` when `None` or empty;
    /// trailing slashes are stripped. `token` is treated as absent when
    /// `None` or empty.
    #[must_use]
    pub(crate) fn from_pair(url: Option<String>, token: Option<String>) -> Self {
        let url = url
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:49374".to_string())
            .trim_end_matches('/')
            .to_string();
        let auth_token = token.filter(|s| !s.is_empty());
        Self { url, auth_token }
    }

    /// Apply auth header to a `reqwest::RequestBuilder` if a token is set.
    pub(crate) fn authenticate(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth_token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }
}

/// GET `<endpoint>{path}` with optional query params, deserialise JSON.
///
/// # Errors
/// Returns an error when the connection fails, the response is non-2xx,
/// or the body can't be deserialised into `T`.
pub async fn get_json<T: DeserializeOwned>(
    endpoint: &ServerEndpoint,
    path: &str,
    query: &[(&str, &str)],
) -> Result<T> {
    let client = reqwest::Client::new();
    let url = format!("{}{path}", endpoint.url);
    let mut req = client.get(&url);
    if !query.is_empty() {
        req = req.query(query);
    }
    req = endpoint.authenticate(req);
    let resp = req
        .send()
        .await
        .map_err(|e| augment_connect_error(e, endpoint, &url))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("server returned {status}: {body}");
    }
    resp.json::<T>()
        .await
        .with_context(|| format!("parsing JSON body from GET {url}"))
}

/// POST JSON body to `<endpoint>{path}`, deserialise JSON response.
///
/// # Errors
/// Same as [`get_json`].
pub async fn post_json<B: Serialize, T: DeserializeOwned>(
    endpoint: &ServerEndpoint,
    path: &str,
    body: &B,
) -> Result<T> {
    let client = reqwest::Client::new();
    let url = format!("{}{path}", endpoint.url);
    let req = endpoint.authenticate(client.post(&url).json(body));
    let resp = req
        .send()
        .await
        .map_err(|e| augment_connect_error(e, endpoint, &url))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("server returned {status}: {body}");
    }
    resp.json::<T>()
        .await
        .with_context(|| format!("parsing JSON body from POST {url}"))
}

/// Turn a low-level reqwest connect/timeout error into a friendlier
/// message that surfaces the env-var configuration. The common case
/// is "Connection refused" — typically because `AI_MEMORY_SERVER_URL`
/// is unset and the CLI defaulted to loopback on a host that has no
/// local server running.
fn augment_connect_error(
    err: reqwest::Error,
    endpoint: &ServerEndpoint,
    url: &str,
) -> anyhow::Error {
    // Walk the source chain to see if there's a Connection-refused
    // io::Error buried somewhere. reqwest wraps its errors deeply.
    let chain_contains_refused = {
        let mut src: Option<&dyn std::error::Error> = Some(&err);
        let mut found = false;
        while let Some(e) = src {
            if e.to_string().contains("Connection refused")
                || e.to_string().contains("connection refused")
            {
                found = true;
                break;
            }
            src = e.source();
        }
        found
    };

    let url_from_env = std::env::var("AI_MEMORY_SERVER_URL").is_ok_and(|v| !v.is_empty());

    if chain_contains_refused {
        let hint = if url_from_env {
            format!(
                "\nAI_MEMORY_SERVER_URL is set to {} but nothing answered. \
                 Check the server is running, the port is reachable from \
                 this host, and (if remote) any firewall + bearer-token \
                 config matches.",
                endpoint.url
            )
        } else {
            format!(
                "\nAI_MEMORY_SERVER_URL is NOT set; the CLI defaulted to \
                 {} and nothing answered. If your server lives on another \
                 machine (e.g. a homelab), `export AI_MEMORY_SERVER_URL=\
                 http://<server>:49374` and (if auth is on) \
                 `export AI_MEMORY_AUTH_TOKEN=<token>` before re-running.",
                endpoint.url
            )
        };
        anyhow::Error::new(err).context(format!("could not reach {url}.{hint}"))
    } else {
        anyhow::Error::new(err).context(format!("HTTP request to {url} failed"))
    }
}

/// POST an empty body to `<endpoint>{path}`, return the raw response bytes.
///
/// Intended for routes whose response is binary (e.g. `POST /admin/backup`
/// returns an `application/gzip` tarball). On non-2xx the response body is
/// consumed and returned as an error string.
///
/// # Errors
/// Returns an error when the connection fails, the response is non-2xx,
/// or the body cannot be read.
pub async fn post_bytes(endpoint: &ServerEndpoint, path: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::new();
    let url = format!("{}{path}", endpoint.url);
    let req = endpoint.authenticate(client.post(&url));
    let resp = req
        .send()
        .await
        .map_err(|e| augment_connect_error(e, endpoint, &url))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("server returned {status}: {body}");
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .with_context(|| format!("reading response bytes from POST {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // ServerEndpoint::from_pair
    // ----------------------------------------------------------------

    #[test]
    fn from_pair_defaults_to_loopback_when_none() {
        let ep = ServerEndpoint::from_pair(None, None);
        assert_eq!(ep.url, "http://127.0.0.1:49374");
        assert!(ep.auth_token.is_none());
    }

    #[test]
    fn from_pair_defaults_to_loopback_when_empty() {
        let ep = ServerEndpoint::from_pair(Some(String::new()), None);
        assert_eq!(ep.url, "http://127.0.0.1:49374");
    }

    #[test]
    fn from_pair_strips_trailing_slash() {
        let ep = ServerEndpoint::from_pair(Some("http://10.0.0.1:8080/".to_string()), None);
        assert_eq!(ep.url, "http://10.0.0.1:8080");
    }

    #[test]
    fn from_pair_strips_multiple_trailing_slashes() {
        let ep = ServerEndpoint::from_pair(Some("http://10.0.0.1:8080///".to_string()), None);
        assert_eq!(ep.url, "http://10.0.0.1:8080");
    }

    #[test]
    fn from_pair_empty_token_treated_as_none() {
        let ep = ServerEndpoint::from_pair(None, Some(String::new()));
        assert!(ep.auth_token.is_none());
    }

    #[test]
    fn from_pair_non_empty_token_preserved() {
        let ep = ServerEndpoint::from_pair(None, Some("secret".to_string()));
        assert_eq!(ep.auth_token.as_deref(), Some("secret"));
    }

    // ----------------------------------------------------------------
    // ServerEndpoint::authenticate
    // ----------------------------------------------------------------

    #[test]
    fn authenticate_no_token_leaves_request_unchanged() {
        let ep = ServerEndpoint::from_pair(None, None);
        let client = reqwest::Client::new();
        // Build a request, authenticate it, then build to inspect.
        let req = ep
            .authenticate(client.get("http://localhost"))
            .build()
            .unwrap();
        // No Authorization header should be present.
        assert!(
            req.headers().get("authorization").is_none(),
            "no Authorization header expected"
        );
    }

    #[test]
    fn authenticate_with_token_sets_bearer_header() {
        let ep = ServerEndpoint::from_pair(None, Some("tok123".to_string()));
        let client = reqwest::Client::new();
        let req = ep
            .authenticate(client.get("http://localhost"))
            .build()
            .unwrap();
        let auth = req
            .headers()
            .get("authorization")
            .expect("Authorization header must be set")
            .to_str()
            .unwrap();
        assert_eq!(auth, "Bearer tok123");
    }
}
