//! SUPERCONDUCTOR-19: signing people in, and the one public port.
//!
//! People sign in through `plexus-login` 0.2 (the Plexus 0.7 line), linked as menger
//! links it: WorkOS by emailed code when a client and its API key are configured
//! (the same client as menger's matching environment, so one person is one
//! `oidc:workos/user_…` id), and key sign-in (`key:<hex>`) always. With no WorkOS
//! key there is no WorkOS button, and admins are named with `--admin key:<hex>`.
//! Sessions (the host-only cookie, `mgs_` tokens) are this service's own.
//!
//! ```text
//!   public port ──┬── GET /            the page (hosts, pairing): page.html
//!                 ├── /login*, /logout, /auth/cli   plexus-login's routes
//!                 └── an upgrade, on any path (/, /ws)  ──▶ 127.0.0.1:<inner>
//!                     Origin checked here: none (a CLI), or this service's own
//! ```
//!
//! plexus-transport 0.6 serves nothing but the socket on its port, so it listens on
//! loopback and this front proxies upgrades to it, adding the client's address to
//! `X-Forwarded-For` (the inner transport trusts loopback and the configured
//! proxies, so the waiting room's per-address limit sees the real client).

use crate::service::{serve_with, Serving, TokenIssuer};
use axum::extract::ws::{Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use plexus_core::plexus::{DynamicHub, SessionValidator};
use plexus_login::simple::SimpleAccess;
use plexus_login::{Branding, LoginOptions, Wired, Wiring};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TMessage;

/// The page: sign in, your hosts, confirm a pairing.
pub const PAGE: &str = include_str!("page.html");

/// The app's name as plexus-login uses it: cookies (`agents_session…`) and
/// `AGENTS_WORKOS_API_KEY`.
pub const BRANDING: Branding = Branding {
    app: "agents",
    version: env!("CARGO_PKG_VERSION"),
    mark: "◇",
    motto: "your hosts, wherever they are",
};

/// How people and services sign in.
#[derive(Debug, Clone, Default)]
pub struct SignInConfig {
    /// Where people reach this service: `https://host`, or the socket's
    /// `wss://host/ws`, which says the same origin.
    pub public_url: Option<String>,
    /// The WorkOS client (`client_…`): menger's, in the matching environment.
    pub workos_client: Option<String>,
    /// A file holding the WorkOS API key (`sk_…`). `AGENTS_WORKOS_API_KEY` also
    /// serves. With neither there's no WorkOS sign-in.
    pub workos_key_file: Option<PathBuf>,
    /// A whole `--identity` spec, in place of the two above.
    pub identity: Option<String>,
    /// plexus-idp tokens, as before.
    pub idp: Option<TokenIssuer>,
    /// More URLs a key proof may name, beside this service's own.
    pub key_audiences: Vec<String>,
    /// Principals that are `admin` everywhere: `key:<hex>`, `oidc:workos/user_…`.
    pub admins: Vec<String>,
    /// CIDRs of the proxies in front, or `none`.
    pub trusted_proxies: Vec<String>,
    /// Where sessions are kept across restarts; in memory if none.
    pub sessions_file: Option<PathBuf>,
    /// Whether the public port listens on loopback only.
    pub loopback: bool,
}

/// `https://host`, `wss://host/ws` or `ws://127.0.0.1:4470` as the page's origin:
/// `https://host`, `http://127.0.0.1:4470`.
pub fn page_origin(url: &str) -> anyhow::Result<String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| anyhow::anyhow!("--public-url `{url}` is not a URL"))?;
    let scheme = match scheme {
        "https" | "wss" => "https",
        "http" | "ws" => "http",
        other => anyhow::bail!("--public-url `{url}`: `{other}` is not http(s) or ws(s)"),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    anyhow::ensure!(!authority.is_empty(), "--public-url `{url}` names no host");
    Ok(format!("{scheme}://{authority}"))
}

/// The socket at `origin`: `wss://host/ws`, `ws://127.0.0.1:4470/ws`.
pub fn socket_url(origin: &str) -> String {
    let ws = origin
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    format!("{ws}/ws")
}

/// Sign-in, wired: the session gate, the `auth` backend, the page's routes.
pub struct Login {
    pub wired: Wired<SimpleAccess>,
    /// The origins whose pages may open the socket.
    pub origins: Vec<String>,
    /// Whether WorkOS sign-in is on.
    pub workos: bool,
}

impl Login {
    /// The `auth` activation's backend, for [`crate::service::hub_with_login`].
    pub fn backend(&self) -> Arc<dyn plexus_core::plexus::AuthBackend> {
        self.wired.backend.clone()
    }

    /// The upgrade's validator: sessions, `mgs_` tokens, then each provider.
    pub fn validator(&self) -> anyhow::Result<Arc<dyn SessionValidator>> {
        let gate = self.wired.gate.clone();
        let gate = gate.ok_or_else(|| anyhow::anyhow!("no way to sign in"))?;
        Ok(gate)
    }
}

/// Wires sign-in for a service on `port`.
pub fn sign_in(config: &SignInConfig, port: u16) -> anyhow::Result<Login> {
    let origin = config.public_url.as_deref().map(page_origin).transpose()?;
    let mut providers = Vec::new();
    providers.push(if config.key_audiences.is_empty() {
        "key".to_string()
    } else {
        format!("key={}", config.key_audiences.join(","))
    });
    if let Some(t) = &config.idp {
        providers.push(format!("idp={},{}", t.issuer, t.audience));
    }
    let identity = match (&config.identity, &config.workos_client) {
        (Some(spec), _) => Some(spec.clone()),
        (None, Some(client)) => {
            let env = plexus_login::backends::workos::ApiKey::env_var(BRANDING.app);
            let in_env = std::env::var(&env).is_ok_and(|v| !v.trim().is_empty());
            match &config.workos_key_file {
                // Emailed codes need no redirect URI; SSO and social would, so off.
                Some(file) => Some(format!("workos:{client},sso=off,key=@{}", file.display())),
                None if in_env => Some(format!("workos:{client},sso=off")),
                None => {
                    eprintln!("plexus-agents: a WorkOS client but no key ({env} or --workos-api-key-file): key sign-in only");
                    None
                }
            }
        }
        (None, None) => None,
    };
    let workos = identity
        .as_deref()
        .is_some_and(|s| s.starts_with("workos:"));
    let mut opts = LoginOptions::new(BRANDING, port, port).with_providers(&providers)?;
    opts.public_url = origin.clone();
    opts.identity = identity
        .as_deref()
        .map(|s| plexus_login::backends::from_spec(s, BRANDING.app))
        .transpose()?;
    if !config.trusted_proxies.is_empty() {
        opts.trusted_proxies =
            plexus_login::client_ip::TrustedProxies::parse(&config.trusted_proxies)
                .map_err(|e| anyhow::anyhow!("--trusted-proxy: {e}"))?;
    }
    opts.sessions_file = config.sessions_file.clone();
    opts.shell_loopback = config.loopback;
    let wiring = Wiring::new(opts)?;
    let access = SimpleAccess::new(wiring.sessions(), &config.admins)?;
    let wired = wiring.finish(access, None)?;
    let mut origins = wired.origins.clone();
    if let Some(o) = origin {
        if !origins.contains(&o) {
            origins.push(o);
        }
    }
    Ok(Login {
        wired,
        origins,
        workos,
    })
}

#[derive(Clone)]
struct Front {
    inner: u16,
    origins: Arc<Vec<String>>,
}

/// Serves `hub` on `serving.port`: the page, sign-in, and the socket. See the
/// module docs.
pub async fn serve_public(
    hub: Arc<DynamicHub>,
    login: Login,
    serving: Serving,
) -> anyhow::Result<()> {
    let inner = {
        let l = std::net::TcpListener::bind("127.0.0.1:0")?;
        l.local_addr()?.port()
    };
    let mut proxies = vec!["127.0.0.1/32".to_string()];
    proxies.extend(
        serving
            .trusted_proxies
            .iter()
            .filter(|p| p.as_str() != "none")
            .cloned(),
    );
    let inside = Serving {
        bind: IpAddr::from([127, 0, 0, 1]),
        port: inner,
        hosts: serving.hosts.clone(),
        trusted_proxies: proxies,
    };
    let validator = login.validator()?;
    let socket = tokio::spawn(serve_with(hub, validator, inside));
    for _ in 0..500 {
        if tokio::net::TcpStream::connect(("127.0.0.1", inner))
            .await
            .is_ok()
        {
            break;
        }
        if socket.is_finished() {
            return socket.await?;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let front = Front {
        inner,
        origins: Arc::new(login.origins.clone()),
    };
    let mut app = axum::Router::new().fallback(handle).with_state(front);
    if let Some(routes) = login.wired.router() {
        app = routes.merge(app);
    }
    if let Some(meta) = login.wired.metadata_route() {
        app = app.merge(meta);
    }
    let listener = tokio::net::TcpListener::bind((serving.bind, serving.port)).await?;
    let served = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    );
    tokio::select! {
        r = served => r.map_err(Into::into),
        r = socket => r?,
    }
}

async fn handle(
    State(front): State<Front>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    let upgrade = req
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    if upgrade {
        return proxy(front, peer, req).await;
    }
    let path = req.uri().path();
    if req.method() == Method::GET && (path == "/" || path == "/index.html") {
        let mut page = Html(PAGE).into_response();
        let h = page.headers_mut();
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(
                "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self' ws: wss:; frame-ancestors 'none'",
            ),
        );
        return page;
    }
    (StatusCode::NOT_FOUND, "not here").into_response()
}

/// Whether a browser page at `origin` may open the socket.
fn origin_allowed(origins: &[String], origin: &str) -> bool {
    let origin = origin.trim_end_matches('/').to_ascii_lowercase();
    origins.iter().any(|o| o.to_ascii_lowercase() == origin)
}

async fn proxy(front: Front, peer: SocketAddr, req: Request) -> Response {
    let headers = req.headers().clone();
    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or_default();
        if !origin_allowed(&front.origins, origin) {
            let why = format!("a page at {origin} may not open this socket");
            return (StatusCode::FORBIDDEN, why).into_response();
        }
    }
    let Ok(mut upstream) = format!("ws://127.0.0.1:{}/", front.inner).into_client_request() else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    for name in [
        header::AUTHORIZATION,
        header::COOKIE,
        header::SEC_WEBSOCKET_PROTOCOL,
        header::USER_AGENT,
    ] {
        if let Some(v) = headers.get(&name) {
            upstream.headers_mut().insert(name, v.clone());
        }
    }
    let forwarded = match headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        Some(before) => format!("{before}, {}", peer.ip()),
        None => peer.ip().to_string(),
    };
    if let Ok(v) = HeaderValue::from_str(&forwarded) {
        upstream.headers_mut().insert("x-forwarded-for", v);
    }
    let (socket, answer) = match tokio_tungstenite::connect_async(upstream).await {
        Ok(ok) => ok,
        Err(tokio_tungstenite::tungstenite::Error::Http(refused)) => {
            let mut out = Response::new(axum::body::Body::from(
                refused.body().clone().unwrap_or_default(),
            ));
            *out.status_mut() = refused.status();
            if let Some(v) = refused.headers().get(header::WWW_AUTHENTICATE) {
                out.headers_mut()
                    .insert(header::WWW_AUTHENTICATE, v.clone());
            }
            return out;
        }
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let protocol = answer
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let (mut parts, _) = req.into_parts();
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(ws) => ws,
        Err(e) => return e.into_response(),
    };
    let ws = match protocol {
        Some(p) => ws.protocols([p]),
        None => ws,
    };
    ws.on_upgrade(move |client| pipe(client, socket))
}

type Upstream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Frames both ways until either side ends. Pings stay with each side.
async fn pipe(client: WebSocket, upstream: Upstream) {
    let (mut to_client, mut from_client) = client.split();
    let (mut to_upstream, mut from_upstream) = upstream.split();
    let up = async {
        while let Some(Ok(m)) = from_client.next().await {
            let m = match m {
                AxMessage::Text(t) => TMessage::Text(t.as_str().into()),
                AxMessage::Binary(b) => TMessage::Binary(b),
                AxMessage::Close(_) => break,
                AxMessage::Ping(_) | AxMessage::Pong(_) => continue,
            };
            if to_upstream.send(m).await.is_err() {
                break;
            }
        }
        let _ = to_upstream.close().await;
    };
    let down = async {
        while let Some(Ok(m)) = from_upstream.next().await {
            let m = match m {
                TMessage::Text(t) => AxMessage::Text(t.as_str().into()),
                TMessage::Binary(b) => AxMessage::Binary(b),
                TMessage::Close(_) => break,
                _ => continue,
            };
            if to_client.send(m).await.is_err() {
                break;
            }
        }
        let _ = to_client.close().await;
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_public_url_in_either_form_is_one_origin() {
        let https = page_origin("https://agents.example").unwrap();
        assert_eq!(https, "https://agents.example");
        assert_eq!(page_origin("wss://agents.example/ws").unwrap(), https);
        assert_eq!(page_origin("https://agents.example/").unwrap(), https);
        assert_eq!(
            page_origin("ws://127.0.0.1:4470").unwrap(),
            "http://127.0.0.1:4470"
        );
        assert_eq!(socket_url(&https), "wss://agents.example/ws");
        assert!(page_origin("agents.example").is_err());
        assert!(page_origin("ftp://agents.example").is_err());
    }

    #[test]
    fn only_listed_origins_may_open_the_socket() {
        let origins = vec!["https://agents.example".to_string()];
        assert!(origin_allowed(&origins, "https://agents.example"));
        assert!(origin_allowed(&origins, "https://AGENTS.example/"));
        assert!(!origin_allowed(&origins, "https://evil.example"));
        assert!(!origin_allowed(&origins, "http://agents.example"));
    }
}
