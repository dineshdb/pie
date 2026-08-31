//! The proxy server: an HTTP forward proxy that enforces an egress [`Policy`].
//!
//! Only the hostname is inspected, never the payload. `CONNECT` targets are
//! read from the request authority and TLS bytes are relayed untouched, so
//! there is no interception, no certificate authority, and nothing to trust.

use crate::policy::{Decision, Policy, Target};
use rama::{
    Layer, Service,
    dns::client::DnsConnector,
    extensions::ExtensionsRef,
    http::{
        Body, Request, Response, StatusCode,
        client::EasyHttpWebClient,
        layer::{
            remove_header::{RemoveRequestHeaderLayer, RemoveResponseHeaderLayer},
            trace::TraceLayer,
            upgrade::{EagerHttpProxyConnector, UpgradeLayer},
        },
        matcher::MethodMatcher,
        server::HttpServer,
    },
    layer::{ConsumeErrLayer, TimeoutLayer},
    net::client::ConnectorTarget,
    net::proxy::IoForwardService,
    rt::Executor,
    service::service_fn,
    tcp::client::service::TcpConnector,
    tcp::server::TcpListener,
};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

/// Default port assumed for an absolute-form request without one.
const DEFAULT_HTTP_PORT: u16 = 80;
/// Default port assumed for a `CONNECT` authority without one.
const DEFAULT_CONNECT_PORT: u16 = 443;
/// Upper bound on establishing the upstream connection.
const UPSTREAM_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Runs the proxy until the process is shut down.
pub async fn serve(listen: SocketAddr, policy: Policy) -> Result<(), BoxError> {
    serve_with_ready(listen, policy, |_| {}).await
}

/// Like [`serve`], but hands the actually bound address to `on_ready`.
///
/// Needed whenever `listen` uses port 0: a test, or piebox asking the OS for a
/// free port and then telling the guest which one to use.
pub async fn serve_with_ready<F>(
    listen: SocketAddr,
    policy: Policy,
    on_ready: F,
) -> Result<(), BoxError>
where
    F: FnOnce(SocketAddr) + Send + 'static,
{
    let policy = Arc::new(policy);
    let graceful = rama::graceful::Shutdown::default();
    graceful.spawn_task_fn(async move |guard| {
        let exec = Executor::graceful(guard);
        let tcp = match TcpListener::build(exec.clone()).bind_address(listen).await {
            Ok(tcp) => tcp,
            Err(err) => {
                tracing::error!(%listen, "failed to bind: {err}");
                return;
            }
        };
        match tcp.local_addr() {
            Ok(bound) => {
                tracing::info!(
                    listen = %bound,
                    mode = %policy.mode(),
                    ports = ?policy.ports(),
                    allow = ?policy.allow_rules().iter().map(ToString::to_string).collect::<Vec<_>>(),
                    deny = ?policy.deny_rules().iter().map(ToString::to_string).collect::<Vec<_>>(),
                    "pie-proxy listening",
                );
                on_ready(bound);
            }
            Err(err) => {
                tracing::error!("bound but could not read local address: {err}");
                return;
            }
        }

        let http = HttpServer::auto(exec.clone()).service(
            (
                TraceLayer::new_for_http(),
                ConsumeErrLayer::default(),
                // Policy sits outside the upgrade layer so a CONNECT is judged
                // before any tunnel exists.
                PolicyLayer::new(policy.clone()),
                UpgradeLayer::new(
                    exec.clone(),
                    MethodMatcher::CONNECT,
                    EagerHttpProxyConnector::new(
                        // A bare TcpConnector only dials IP addresses, so the
                        // DNS connector is what makes hostnames work at all.
                        TimeoutLayer::new(UPSTREAM_CONNECT_TIMEOUT)
                            .into_layer(DnsConnector::new(TcpConnector::new())),
                        IoForwardService::new(exec.clone()),
                    ),
                ),
                RemoveResponseHeaderLayer::hop_by_hop(),
                RemoveRequestHeaderLayer::hop_by_hop(),
            )
                .into_layer(service_fn(forward_plain_http)),
        );

        tcp.serve(http).await;
    });

    graceful
        .shutdown_with_limit(std::time::Duration::from_secs(30))
        .await?;
    Ok(())
}

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Forwards a non-`CONNECT` (plaintext) request upstream.
async fn forward_plain_http(req: Request) -> Result<Response, Infallible> {
    let client = EasyHttpWebClient::default();
    match client.serve(req).await {
        Ok(resp) => Ok(resp),
        Err(err) => {
            tracing::error!("upstream request failed: {err:?}");
            Ok(text_response(
                StatusCode::BAD_GATEWAY,
                "pie-proxy: upstream request failed",
            ))
        }
    }
}

/// Rejects requests whose target the policy denies.
#[derive(Debug, Clone)]
pub struct PolicyLayer {
    policy: Arc<Policy>,
}

impl PolicyLayer {
    pub fn new(policy: Arc<Policy>) -> Self {
        Self { policy }
    }
}

impl<S> Layer<S> for PolicyLayer {
    type Service = PolicyService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        PolicyService {
            policy: self.policy.clone(),
            inner,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PolicyService<S> {
    policy: Arc<Policy>,
    inner: S,
}

impl<S> Service<Request> for PolicyService<S>
where
    S: Service<Request, Output = Response>,
{
    type Output = Response;
    type Error = S::Error;

    async fn serve(&self, mut req: Request) -> Result<Self::Output, Self::Error> {
        let method = req.method().clone();
        let target = match target_of(&req) {
            Ok(target) => target,
            Err(reason) => {
                tracing::warn!(%method, uri = %req.uri(), "rejected unparsable target: {reason}");
                return Ok(text_response(
                    StatusCode::BAD_REQUEST,
                    "pie-proxy: could not determine request target",
                ));
            }
        };

        match self.policy.evaluate(&target) {
            Decision::Allow => {
                tracing::debug!(%method, %target, "allowed");
                // Pin the destination to exactly what was judged. Without
                // this, the connector re-derives it from the URI through a
                // different code path, and any disagreement between the two
                // (userinfo, an implied port) is a policy bypass.
                let pinned = match rama::net::address::HostWithPort::try_from(target.clone()) {
                    Ok(pinned) => pinned,
                    Err(err) => {
                        tracing::warn!(%method, %target, "cannot pin target: {err}");
                        return Ok(text_response(
                            StatusCode::BAD_REQUEST,
                            "pie-proxy: unusable request target",
                        ));
                    }
                };
                req.extensions().insert(ConnectorTarget(pinned));
                // Align the Host header with the authority that was judged.
                // Left alone, a request to an allowed host can carry any Host
                // it likes and select a different origin behind a CDN or any
                // shared-hosting entry in the allow list.
                if method != rama::http::Method::CONNECT
                    && let Ok(value) = rama::http::HeaderValue::from_str(&target.to_string())
                {
                    req.headers_mut().insert(rama::http::header::HOST, value);
                }
                self.inner.serve(req).await
            }
            Decision::Deny(reason) => {
                // Logged at info: a blocked egress attempt is the signal this
                // proxy exists to produce.
                tracing::info!(%method, %target, %reason, "denied");
                Ok(text_response(
                    StatusCode::FORBIDDEN,
                    &format!("pie-proxy: {target} denied ({reason})"),
                ))
            }
        }
    }
}

/// Extracts the connection target from a proxied request.
///
/// `CONNECT` carries an authority; a forward-proxied plaintext request carries
/// an absolute URI; anything else falls back to the `Host` header.
fn target_of(req: &Request) -> Result<Target, String> {
    let uri = req.uri();
    if req.method() == rama::http::Method::CONNECT {
        // A CONNECT line carries an authority-form target. Depending on how the
        // request was constructed the URI type may not expose it as an
        // authority, so fall back to the raw form rather than failing open.
        let authority = match uri.authority() {
            Some(authority) => authority.to_string(),
            None => uri.to_string(),
        };
        // No default port: a CONNECT line that omits it is malformed, and
        // guessing would risk checking one port while dialling another.
        return Target::parse_authority(&authority, None)
            .map_err(|err| format!("CONNECT target {authority:?}: {err}"));
    }

    if let Some(authority) = uri.authority() {
        let default_port = default_port_for_scheme(uri.scheme_str());
        return Target::parse_authority(&authority.to_string(), Some(default_port))
            .map_err(|err| err.to_string());
    }

    let host = req
        .headers()
        .get(rama::http::header::HOST)
        .ok_or_else(|| "origin-form request without Host header".to_string())?
        .to_str()
        .map_err(|_| "Host header is not valid text".to_string())?;
    Target::parse_authority(host, Some(default_port_for_scheme(uri.scheme_str())))
        .map_err(|err| err.to_string())
}

fn default_port_for_scheme(scheme: Option<&str>) -> u16 {
    match scheme {
        Some("https") => DEFAULT_CONNECT_PORT,
        _ => DEFAULT_HTTP_PORT,
    }
}

fn text_response(status: StatusCode, message: &str) -> Response {
    Response::builder()
        .status(status)
        .header(
            rama::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )
        .body(Body::from(format!("{message}\n")))
        // A refusal must still be delivered even if the builder ever objects.
        .unwrap_or_else(|_| Response::new(Body::from("pie-proxy: request refused\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Host;

    fn request(method: &str, uri: &str) -> Request {
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn connect_target_comes_from_the_authority() {
        let target = target_of(&request("CONNECT", "github.com:443")).unwrap();
        assert_eq!(target.host, Host::Domain("github.com".into()));
        assert_eq!(target.port, 443);
    }

    // Malformed authorities (portless CONNECT, embedded userinfo,
    // percent-encoded hosts) cannot be expressed through `Request::builder`,
    // whose URI parser rejects them outright. They are only reachable over the
    // wire, so they are tested there — see tests/tunnel.rs, in particular
    // `the_checked_target_is_the_connected_target` and
    // `portless_connect_is_refused`.

    #[test]
    fn absolute_form_target_uses_scheme_default_port() {
        let target = target_of(&request("GET", "http://example.com/x")).unwrap();
        assert_eq!(target.host, Host::Domain("example.com".into()));
        assert_eq!(target.port, 80);

        let target = target_of(&request("GET", "https://example.com/x")).unwrap();
        assert_eq!(target.port, 443);
    }

    #[test]
    fn absolute_form_explicit_port_wins() {
        let target = target_of(&request("GET", "http://example.com:8080/x")).unwrap();
        assert_eq!(target.port, 8080);
    }

    #[test]
    fn origin_form_falls_back_to_host_header() {
        let req = Request::builder()
            .method("GET")
            .uri("/index.html")
            .header("host", "example.com:8080")
            .body(Body::empty())
            .unwrap();
        let target = target_of(&req).unwrap();
        assert_eq!(target.host, Host::Domain("example.com".into()));
        assert_eq!(target.port, 8080);
    }

    #[test]
    fn origin_form_without_host_header_is_rejected() {
        assert!(target_of(&request("GET", "/index.html")).is_err());
    }
}
