//! OAuth 2.1 authorization for MCP servers ([`crate::config::McpAuthConfig`]),
//! on top of rmcp's `transport::auth`.
//!
//! The browser flow (metadata discovery, dynamic or pre-registered client,
//! authorization code + PKCE) runs once per server via `pie mcp login`: a
//! localhost listener catches the redirect, the token set lands in
//! `~/.pie/pie.db`, and every later run authorizes from the store through
//! [`authorization_manager`] — rmcp refreshes and retries transparently
//! behind `McpPlugin::add_remote_server_authorized`.

use crate::config::McpServerConfig;
use crate::db::DbPool;
use crate::error::{AppError, Result};
use chrono::Utc;
use rmcp::transport::auth::{
    AuthError, AuthorizationCallback, AuthorizationManager, AuthorizationRequest,
    AuthorizationSession, CredentialStore, StoredCredentials,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How long `pie mcp login` waits for the browser redirect.
const CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// An [`AuthorizationManager`] wired to the stored credentials of one
/// configured MCP server, plus whether a token was actually stored. With no
/// stored token the manager still works — rmcp sends requests
/// unauthenticated, so open servers connect as usual and OAuth servers
/// answer 401, which is the cue to run `pie mcp login`.
pub async fn authorization_manager(
    name: &str,
    server: &McpServerConfig,
    pool: DbPool,
) -> Result<(AuthorizationManager, bool)> {
    let mut manager = AuthorizationManager::new(server.url.as_str())
        .await
        .map_err(auth_err(name))?;
    manager.set_credential_store(SqliteCredentialStore::new(pool, name.to_string()));
    let stored = manager
        .initialize_from_store()
        .await
        .map_err(auth_err(name))?;
    Ok((manager, stored))
}

/// Run the interactive browser flow for `name` and store the resulting
/// tokens. Returns the granted scopes. Works for any configured server —
/// `[mcp.<name>.auth]` only overrides the defaults (pre-registered
/// credentials, scopes, fixed callback port).
pub async fn login(name: &str, server: &McpServerConfig, pool: DbPool) -> Result<Vec<String>> {
    let listener =
        bind_callback_listener(server.auth.as_ref().and_then(|a| a.redirect_port)).await?;
    let manager = AuthorizationManager::new(server.url.as_str())
        .await
        .map_err(auth_err(name))?;
    run_login(name, manager, server, listener, pool, |url| {
        println!("Open this URL to authorize {name}:\n\n  {url}\n\nWaiting for the browser callback (Ctrl-C aborts)…");
        if let Err(e) = open::that(url) {
            tracing::warn!("could not open a browser ({e}); copy the URL manually");
        }
    })
    .await
}

/// Forget the stored tokens for `name`.
pub async fn logout(name: &str, pool: DbPool) -> Result<()> {
    SqliteCredentialStore::new(pool, name.to_string())
        .clear()
        .await
        .map_err(auth_err(name))?;
    println!("forgot stored OAuth tokens for {name}");
    Ok(())
}

/// Bind the local OAuth callback listener — an exact port when configured
/// (some servers only accept pre-registered redirect URIs), otherwise
/// ephemeral.
pub(crate) async fn bind_callback_listener(port: Option<u16>) -> Result<TcpListener> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port.unwrap_or(0)));
    TcpListener::bind(addr).await.map_err(Into::into)
}

/// The login flow proper, split from [`login`] so tests can drive it with
/// a stubbed OAuth HTTP client and a pre-bound listener. `announce` shows
/// the authorization URL (print + browser in the CLI, capture in tests).
pub(crate) async fn run_login(
    name: &str,
    mut manager: AuthorizationManager,
    server: &McpServerConfig,
    listener: TcpListener,
    pool: DbPool,
    announce: impl Fn(&str),
) -> Result<Vec<String>> {
    manager.set_credential_store(SqliteCredentialStore::new(pool.clone(), name.to_string()));

    let resolution = manager
        .resolve_metadata_from_challenge(None)
        .await
        .map_err(auth_err(name))?;
    manager.set_metadata(resolution.metadata);

    let port = listener.local_addr()?.port();
    let mut request = AuthorizationRequest::new(format!("http://127.0.0.1:{port}/callback"))
        .with_client_name("pie");
    // The section is optional: present, it overrides the defaults
    // (pre-registered credentials instead of dynamic registration, explicit
    // scopes, and login() already took its redirect_port).
    if let Some(auth) = &server.auth {
        if let Some(client_id) = &auth.client_id {
            request = request.with_preregistered_client(client_id);
            if let Some(secret) = &auth.client_secret {
                request = request.with_client_secret(secret.expose_secret());
            }
        }
        if !auth.scopes.is_empty() {
            request = request.with_scopes(auth.scopes.clone());
        }
    }

    let session = AuthorizationSession::new(manager, request)
        .await
        .map_err(|(_, e)| auth_err(name)(e))?;
    announce(&session.auth_url);

    let redirect = tokio::time::timeout(CALLBACK_TIMEOUT, wait_for_callback(&listener))
        .await
        .map_err(|_| AppError::Plugin("timed out waiting for the OAuth callback".into()))??;
    let full_url = format!("http://127.0.0.1:{port}{redirect}");
    let callback = AuthorizationCallback::from_redirect_url(&full_url).map_err(auth_err(name))?;
    session
        .handle_callback_with_issuer(
            &callback.code,
            &callback.csrf_token,
            callback.issuer.as_deref(),
        )
        .await
        .map_err(auth_err(name))?;

    let stored = SqliteCredentialStore::new(pool, name.to_string())
        .load()
        .await
        .map_err(auth_err(name))?
        .ok_or_else(|| AppError::Plugin("login finished but no token was stored".into()))?;
    tracing::info!(server = name, issuer = ?stored.issuer, "mcp oauth login complete");
    Ok(stored.granted_scopes)
}

/// Accept one browser redirect and return the request URI
/// (`/callback?code=…&state=…`).
async fn wait_for_callback(listener: &TcpListener) -> Result<String> {
    let (mut socket, _) = listener.accept().await?;
    // Zero-initialized, so the unread tail past a short read is NULs that
    // the first-line parse never reaches.
    let mut buf = vec![0u8; 8192];
    let _ = socket.read(&mut buf).await?;
    let request_uri = String::from_utf8_lossy(&buf)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(str::to_string);

    let body = "<html><body><h2>pie: authorization complete</h2>\
                <p>You can close this window and return to the terminal.</p></body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;

    request_uri.ok_or_else(|| AppError::Plugin("malformed OAuth callback request".into()))
}

fn auth_err(name: &str) -> impl Fn(AuthError) -> AppError + '_ {
    move |e| AppError::Plugin(format!("mcp '{name}': oauth: {e}"))
}

/// rmcp's [`CredentialStore`] backed by the `mcp_oauth_tokens` table — one
/// row per configured server, holding `StoredCredentials` as JSON.
struct SqliteCredentialStore {
    pool: DbPool,
    server: String,
}

impl SqliteCredentialStore {
    fn new(pool: DbPool, server: String) -> Self {
        Self { pool, server }
    }
}

#[async_trait::async_trait]
impl CredentialStore for SqliteCredentialStore {
    async fn load(&self) -> std::result::Result<Option<StoredCredentials>, AuthError> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT credentials FROM mcp_oauth_tokens WHERE server_name = ?")
                .bind(&self.server)
                .fetch_optional(&self.pool)
                .await
                .map_err(store_err)?;
        row.map(|(json,)| serde_json::from_str(&json).map_err(store_err))
            .transpose()
    }

    async fn save(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        let json = serde_json::to_string(&credentials).map_err(store_err)?;
        sqlx::query(
            "INSERT INTO mcp_oauth_tokens (server_name, credentials, updated_at) \
             VALUES (?, ?, ?) \
             ON CONFLICT(server_name) DO UPDATE SET \
             credentials = excluded.credentials, updated_at = excluded.updated_at",
        )
        .bind(&self.server)
        .bind(&json)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(store_err)?;
        Ok(())
    }

    async fn clear(&self) -> std::result::Result<(), AuthError> {
        sqlx::query("DELETE FROM mcp_oauth_tokens WHERE server_name = ?")
            .bind(&self.server)
            .execute(&self.pool)
            .await
            .map_err(store_err)?;
        Ok(())
    }
}

fn store_err(e: impl std::fmt::Display) -> AuthError {
    AuthError::CredentialStoreError(e.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::config::McpAuthConfig;
    use crate::config::McpServerConfig;
    use rmcp::transport::auth::{OAuthHttpClient, OAuthHttpClientFuture, OAuthHttpRequest};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn server_with(auth: McpAuthConfig) -> McpServerConfig {
        McpServerConfig {
            url: "https://mcp.example.test/mcp".parse().unwrap(),
            api_key: None,
            headers: HashMap::default(),
            auth: Some(auth),
            main_agent_only: false,
        }
    }

    /// Serves the discovery documents, registration and token endpoints of a
    /// fake authorization server; every request is answered from canned JSON
    /// keyed by URI substring, so no network leaves the process.
    struct StubAuthServer;

    impl OAuthHttpClient for StubAuthServer {
        fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
            let uri = request.request.uri().to_string();
            let body = if uri.contains("oauth-protected-resource") {
                serde_json::json!({
                    "resource": "https://mcp.example.test",
                    "authorization_servers": ["https://auth.example.test"],
                    "scopes_supported": ["read", "write"]
                })
            } else if uri.contains("well-known") {
                serde_json::json!({
                    "issuer": "https://auth.example.test",
                    "authorization_endpoint": "https://auth.example.test/authorize",
                    "token_endpoint": "https://auth.example.test/token",
                    "registration_endpoint": "https://auth.example.test/register",
                    "response_types_supported": ["code"],
                    "code_challenge_methods_supported": ["S256"],
                    "grant_types_supported": ["authorization_code", "refresh_token"],
                    "token_endpoint_auth_methods_supported": ["none", "client_secret_basic"],
                    "scopes_supported": ["read", "write", "offline_access"]
                })
            } else if uri.contains("/register") {
                serde_json::json!({
                    "client_id": "dcr-client-123",
                    "client_id_issued_at": 1,
                    "redirect_uris": ["http://127.0.0.1:0/callback"],
                    "token_endpoint_auth_method": "none",
                    "grant_types": ["authorization_code", "refresh_token"],
                    "response_types": ["code"]
                })
            } else if uri.contains("/token") {
                serde_json::json!({
                    "access_token": "at-123",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "refresh_token": "rt-123",
                    "scope": "read write"
                })
            } else {
                serde_json::json!({ "error": "unexpected oauth request", "uri": uri })
            };
            Box::pin(async move {
                Ok(oauth2::http::Response::builder()
                    .status(200)
                    .body(serde_json::to_vec(&body).unwrap())
                    .unwrap())
            })
        }
    }

    fn extract_param(url: &str, param: &str) -> Option<String> {
        url.split('?').nth(1)?.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == param).then(|| v.to_string())
        })
    }

    #[tokio::test]
    async fn login_flow_registers_dynamically_and_persists_tokens() {
        let pool = crate::db::create_test_pool().await.unwrap();
        let server = server_with(McpAuthConfig {
            scopes: vec!["read".to_string(), "write".to_string()],
            ..Default::default()
        });

        let listener = bind_callback_listener(None).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let manager = AuthorizationManager::new_with_oauth_http_client(
            server.url.as_str(),
            Arc::new(StubAuthServer),
        )
        .await
        .unwrap();

        let auth_urls: Arc<Mutex<Vec<String>>> = Arc::default();
        let flow_urls = auth_urls.clone();
        let flow = run_login(
            "linear",
            manager,
            &server,
            listener,
            pool.clone(),
            move |url| flow_urls.lock().unwrap().push(url.to_string()),
        );

        // Play the browser: as soon as the flow publishes the authorization
        // URL, hit the local callback with a forged redirect.
        let browser = tokio::spawn(async move {
            loop {
                let state = auth_urls
                    .lock()
                    .unwrap()
                    .first()
                    .and_then(|url| extract_param(url, "state"));
                if let Some(state) = state {
                    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
                        .await
                        .unwrap();
                    sock.write_all(
                        format!("GET /callback?code=the-code&state={state} HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n")
                            .as_bytes(),
                    )
                    .await
                    .unwrap();
                    sock.shutdown().await.unwrap();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });

        let granted = flow.await.unwrap();
        browser.await.unwrap();

        assert!(
            granted.contains(&"read".to_string()),
            "granted: {granted:?}"
        );
        assert!(
            granted.contains(&"write".to_string()),
            "granted: {granted:?}"
        );

        let row: (String,) =
            sqlx::query_as("SELECT credentials FROM mcp_oauth_tokens WHERE server_name = 'linear'")
                .fetch_one(&pool)
                .await
                .unwrap();
        // StoredCredentials is rmcp's serde type; assert on its JSON rather
        // than re-walking oauth2's accessors.
        assert!(
            row.0.contains("\"client_id\":\"dcr-client-123\""),
            "{}",
            row.0
        );
        assert!(row.0.contains("at-123"), "{}", row.0);
        assert!(row.0.contains("rt-123"), "{}", row.0);
    }

    #[tokio::test]
    async fn authorization_manager_reports_missing_token_without_failing() {
        let pool = crate::db::create_test_pool().await.unwrap();
        let server = server_with(McpAuthConfig::default());

        // No stored token is not an error: the manager still connects
        // servers unauthenticated and lets OAuth servers challenge.
        let (_manager, stored) = authorization_manager("linear", &server, pool)
            .await
            .expect("manager builds without stored credentials");
        assert!(!stored);
    }

    #[tokio::test]
    async fn login_works_without_an_auth_section() {
        let pool = crate::db::create_test_pool().await.unwrap();
        // OAuth is assumed: no [mcp.<name>.auth] at all, defaults everywhere.
        let server = server_with(McpAuthConfig::default());
        let server = McpServerConfig {
            auth: None,
            ..server
        };

        let listener = bind_callback_listener(None).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let manager = AuthorizationManager::new_with_oauth_http_client(
            server.url.as_str(),
            Arc::new(StubAuthServer),
        )
        .await
        .unwrap();

        let auth_urls: Arc<Mutex<Vec<String>>> = Arc::default();
        let flow_urls = auth_urls.clone();
        let flow = run_login(
            "board",
            manager,
            &server,
            listener,
            pool.clone(),
            move |url| flow_urls.lock().unwrap().push(url.to_string()),
        );

        let browser = tokio::spawn(async move {
            loop {
                let state = auth_urls
                    .lock()
                    .unwrap()
                    .first()
                    .and_then(|url| extract_param(url, "state"));
                if let Some(state) = state {
                    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
                        .await
                        .unwrap();
                    sock.write_all(
                        format!(
                            "GET /callback?code=the-code&state={state} HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                    sock.shutdown().await.unwrap();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });

        let granted = flow.await.unwrap();
        browser.await.unwrap();
        assert!(
            granted.contains(&"read".to_string()),
            "granted: {granted:?}"
        );

        let row: (String,) =
            sqlx::query_as("SELECT credentials FROM mcp_oauth_tokens WHERE server_name = 'board'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(row.0.contains("at-123"), "{}", row.0);
    }
}
