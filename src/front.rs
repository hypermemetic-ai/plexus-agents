//! SUPERCONDUCTOR-19, -11, -25: signing people in, and the one public port.
//!
//! People sign in through `plexus-login` 0.3 (the Plexus 0.8 line), linked as menger
//! links it: WorkOS when a client and its API key are configured (the same client
//! as menger's matching environment, so one person is one `oidc:workos/user_…` id),
//! and key sign-in (`key:<hex>`) always, for CLIs, agents and hosts. WorkOS is
//! **GitHub only** (`social=github,email=off,sso=off`), and an allow list
//! (SUPERCONDUCTOR-25, `--allow github:<numeric id>`) says who gets a session: a
//! WorkOS sign-in is admitted by its linked GitHub account's numeric id, and with any
//! rule set a WorkOS bearer token is refused at the socket. WorkOS on a public URL
//! with no rule refuses to start. Sessions (the host-only cookie, `mgs_` tokens) are
//! this service's own.
//!
//! ```text
//!   public port ──┬── an upgrade, on any path (/, /ws)  ──▶ 127.0.0.1:<inner>
//!                 │   Origin checked here: none (a CLI), or this service's own
//!                 ├── /login*, /logout, /auth/cli       plexus-login's routes
//!                 └── GET /, /app/, /app/<hash>/…        plexus-ui's shell: the live
//!                                                        bundle, from `ui.db`
//! ```
//!
//! The page is no part of this crate (SUPERCONDUCTOR-11): it is a plexus-ui bundle
//! (`ui/` in the repository) pushed with `plexus-ui push` and served from the store
//! by plexus-ui-service's routes, stamped with this port as the socket's
//! (`{{WS_PORT}}`) and `plexus-agents` as the service (`{{SERVICE}}`). Until a push
//! the shell answers plexus-ui's own placeholder.
//!
//! plexus-transport serves nothing but the socket on its port, so it listens on
//! loopback and this front proxies upgrades to it, adding the client's address to
//! `X-Forwarded-For` (the inner transport trusts loopback and the configured
//! proxies, so the waiting room's per-address limit sees the real client).

use crate::service::{serve_with, Serving, TokenIssuer, SERVICE};
use axum::extract::ws::{Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use plexus_login::simple::SimpleAccess;
use plexus_login::{Allow, Branding, LoginOptions, Wired, Wiring};
use plexus_rpc::core::plexus::{DynamicHub, SessionValidator};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as TMessage;

/// The app's name as plexus-login uses it: cookies (`agents_session…`) and
/// `AGENTS_WORKOS_API_KEY`.
pub const BRANDING: Branding = Branding {
    app: "agents",
    version: env!("CARGO_PKG_VERSION"),
    mark: "◇",
    motto: "your hosts, wherever they are",
};

/// The shell's title, stamped as `{{TITLE}}`.
pub const TITLE: &str = "superconductor";

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
    /// SUPERCONDUCTOR-25: `--allow` rules, `github:<numeric id>` or
    /// `principal:<principal>`, each one or several comma-separated. None: everyone
    /// gets a session, which WorkOS on a public URL refuses to start with.
    pub allow: Vec<String>,
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

/// Whether an origin from [`page_origin`] names this machine only: `127.0.0.1`,
/// `[::1]`, `localhost`, with any port.
pub fn loopback_origin(origin: &str) -> bool {
    let authority = origin.split_once("://").map_or(origin, |(_, a)| a);
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Sign-in, wired: the session gate, the `auth` backend, the page's routes.
pub struct Login {
    pub wired: Wired<SimpleAccess>,
    /// The origins whose pages may open the socket.
    pub origins: Vec<String>,
    /// Whether WorkOS sign-in is on.
    pub workos: bool,
    /// Who may have a session (SUPERCONDUCTOR-25): everyone when empty.
    pub allow: Allow,
}

impl Login {
    /// The `auth` activation's backend, for [`crate::service::hub_with_login`].
    pub fn backend(&self) -> Arc<dyn plexus_rpc::core::plexus::AuthBackend> {
        self.wired.backend.clone()
    }

    /// The upgrade's validator: sessions, `mgs_` tokens, then each provider.
    pub fn validator(&self) -> anyhow::Result<Arc<dyn SessionValidator>> {
        let gate = self.wired.gate.clone();
        let gate = gate.ok_or_else(|| anyhow::anyhow!("no way to sign in"))?;
        Ok(gate)
    }
}

/// The identity spec for WorkOS sign-in by GitHub alone (SUPERCONDUCTOR-25):
/// no emailed code, no SSO, and the API key from `key_file`, else from the
/// environment (`key` absent).
pub fn github_only(client: &str, key_file: Option<&std::path::Path>) -> String {
    let key = key_file.map_or(String::new(), |f| format!(",key=@{}", f.display()));
    format!("workos:{client},social=github,email=off,sso=off{key}")
}

/// Wires sign-in for a service on `port`. Refuses WorkOS on a public URL with no
/// `--allow` rule, and a rule that doesn't parse.
pub fn sign_in(config: &SignInConfig, port: u16) -> anyhow::Result<Login> {
    let origin = config.public_url.as_deref().map(page_origin).transpose()?;
    let allow = Allow::parse(&config.allow).map_err(|e| anyhow::anyhow!("{e}"))?;
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
                // GitHub alone: AuthKit's social sign-in, which comes back to
                // `<public url>/login/callback` (a redirect URI WorkOS must list).
                Some(file) => Some(github_only(client, Some(file))),
                None if in_env => Some(github_only(client, None)),
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
    let public = origin
        .as_deref()
        .map_or(!config.loopback, |o| !loopback_origin(o));
    if workos && public && allow.is_empty() {
        anyhow::bail!(
            "WorkOS sign-in on {} with no allow rule would give anyone with a WorkOS \
             account a session: name who may sign in with --allow github:<numeric id> \
             (AGENTS_ALLOW), or sign in by key alone",
            origin.as_deref().unwrap_or("a public address")
        );
    }
    let mut opts = LoginOptions::new(BRANDING, port, port)
        .with_providers(&providers)?
        .allowing(allow.clone());
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
        allow,
    })
}

#[derive(Clone)]
struct Front {
    inner: u16,
    origins: Arc<Vec<String>>,
}

/// Serves `hub` on `serving.port`: the socket, sign-in, and the shell. See the
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
        ui: None,
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
    let mut app = shell(&login, serving.ui.clone(), serving.port).await;
    if let Some(routes) = login.wired.router() {
        app = app.merge(routes);
    }
    if let Some(meta) = login.wired.metadata_route() {
        app = app.merge(meta);
    }
    // An upgrade is the socket's, on whatever path it names, before any route.
    let app = app.layer(axum::middleware::from_fn_with_state(front, upgrades));
    let listener = tokio::net::TcpListener::bind((serving.bind, serving.port)).await?;
    let served = plexus_webapp::serve_on(listener, app, std::future::pending());
    tokio::select! {
        r = served => r.map_err(Into::into),
        r = socket => r?,
    }
}

/// The shell: `/`, `/app/` and `/app/<hash>/…` from the ui store (plexus-ui's
/// routes, stamped with this port and service, under plexus-webapp's page
/// policy), the session read from the login's cookie so a preview is that
/// session's. With no store, plexus-webapp's own page saying nothing is built.
async fn shell(
    login: &Login,
    ui: Option<Arc<plexus_ui_service::UiStore>>,
    port: u16,
) -> axum::Router {
    let mut cfg = plexus_webapp::ShellConfig::new(TITLE, plexus_webapp::AppSource::Embedded(&[]));
    cfg.stamps.push(("{{SERVICE}}".into(), SERVICE.into()));
    let (stamps, csp) = (cfg.stamps_for(port), cfg.csp.clone());
    let base = plexus_webapp::router(cfg, port).await;
    let Some(store) = ui else {
        return base;
    };
    let sessions = login.wired.sessions.clone();
    plexus_ui_service::routes::UiRoutes::new(store, port)
        .session(move |h: &axum::http::HeaderMap| {
            let cookies = h.get(header::COOKIE)?.to_str().ok()?;
            sessions.get(plexus_login::sessions::cookie(cookies, sessions.cookie_name())?)
        })
        .stamps(stamps)
        .csp(csp)
        .mount(base)
}

fn is_upgrade(req: &Request) -> bool {
    req.headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
}

async fn upgrades(
    State(front): State<Front>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    if !is_upgrade(&req) {
        return next.run(req).await;
    }
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or(SocketAddr::from(([127, 0, 0, 1], 0)), |c| c.0);
    proxy(front, peer, req).await
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
    fn loopback_origins_are_this_machine_only() {
        for o in ["http://127.0.0.1:4470", "http://localhost:9", "http://[::1]:4470", "http://127.0.0.1"] {
            assert!(loopback_origin(o), "{o}");
        }
        for o in ["https://agents-test.menger.sh", "http://0.0.0.0:4470", "http://10.0.0.1:4470", "https://localhost.example"] {
            assert!(!loopback_origin(o), "{o}");
        }
    }

    fn workos_on(public_url: &str, allow: &[&str]) -> anyhow::Result<Login> {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("workos-api-key");
        std::fs::write(&key, "sk_test_00000000000000000000000000").unwrap();
        let config = SignInConfig {
            public_url: Some(public_url.into()),
            workos_client: Some("client_01TESTTESTTESTTESTTESTTEST".into()),
            workos_key_file: Some(key),
            allow: allow.iter().map(|s| s.to_string()).collect(),
            trusted_proxies: vec!["none".into()],
            ..SignInConfig::default()
        };
        sign_in(&config, 4470)
    }

    #[test]
    fn workos_on_a_public_url_with_no_allow_rule_refuses_to_start() {
        let e = workos_on("https://agents.example", &[]).err().expect("refused");
        assert!(e.to_string().contains("--allow github:"), "{e}");
        // A rule, and it starts, GitHub alone, admitting by the rule.
        let login = workos_on("https://agents.example", &["github:251524973"]).unwrap();
        assert!(login.workos);
        assert_eq!(login.allow.to_string(), "github:251524973");
        assert!(login.allow.wants_github());
        // On this machine alone, no rule is needed.
        assert!(workos_on("http://127.0.0.1:4470", &[]).unwrap().allow.is_empty());
        // A bad rule refuses the whole list: a typo never admits everyone.
        let e = workos_on("https://agents.example", &["github:hypermemetic"]).err().expect("refused");
        assert!(e.to_string().contains("numeric id"), "{e}");
    }

    #[test]
    fn the_workos_sign_in_is_github_alone() {
        let spec = github_only("client_01X", Some(std::path::Path::new("/run/secrets/workos")));
        assert_eq!(spec, "workos:client_01X,social=github,email=off,sso=off,key=@/run/secrets/workos");
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("workos-api-key");
        std::fs::write(&key, "sk_test_00000000000000000000000000").unwrap();
        let spec = github_only("client_01X", Some(&key));
        let backend = plexus_login::backends::from_spec(&spec, BRANDING.app).unwrap();
        let can = backend.capabilities();
        assert_eq!(can.social, vec![plexus_login::identity::Social::GitHub]);
        assert!(!can.email_code && !can.sso_by_domain && !can.password, "{can:?}");
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
