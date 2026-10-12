//! Auditable Rust replacement for the Stremio streaming server (`server.js`).
//!
//! Library-first: [`router`] returns an [`axum::Router`] the eventual native
//! host mounts in-process. [`serve`] is a convenience for the standalone bin
//! and for the oracle-diff tests.
//!
//! Milestone status lives in `docs/streaming-server-rust/` and
//! `checklists/streaming-server-rust.md`. This is **M0** - the control plane.

mod cache_api;
mod hlsv2;
mod local_addon;
mod proxy;
mod routes;
mod security;
mod ssrf;
mod stats;
mod stream;
mod support;
mod torrent;

pub mod config;
pub mod engine;
pub mod storage;
pub mod types;

pub use config::Config;
pub use engine::Engine;

// Re-exported so an embedder can name the [`bind_and_serve`] router type and
// drive it in-process (`tower::ServiceExt::oneshot`) without declaring its own
// axum/tower dependency, which would have to match these versions exactly or the
// types silently stop lining up.
pub use axum;
pub use tower;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::FromRef;
use axum::routing::{any, get, on, post, MethodFilter};
use axum::Router;

/// Shared router state. `FromRef` lets control-plane handlers extract
/// `State<Config>` and torrent handlers extract `State<Engine>` from the same
/// state without either knowing about the other. Outbound HTTP clients are built
/// per-request, pinned to a vetted IP (see [`ssrf`]), so no client lives here.
#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub engine: Engine,
}

impl FromRef<AppState> for Config {
    fn from_ref(s: &AppState) -> Config {
        s.config.clone()
    }
}
impl FromRef<AppState> for Engine {
    fn from_ref(s: &AppState) -> Engine {
        s.engine.clone()
    }
}

/// Build the streaming-server router.
///
/// The socket binds loopback, but that alone does not stop other browsers on the
/// machine from reaching it, so the web-origin trust boundary is enforced here:
/// an Origin allowlist (see [`security`]) rejects real websites while allowing the
/// Tauri webview and no-Origin media/native loads, and every state-changing route
/// is POST-only. Do not bind this to a public interface.
pub fn router(config: Config, engine: Engine) -> Router {
    let state = AppState { config, engine };
    Router::new()
        // M0 control plane
        .route("/settings", get(routes::get_settings).post(routes::post_settings))
        // Rillio-specific torrent prefs (the "faster downloads" toggle). Kept off
        // the Stremio-schema /settings so its oracle diff stays clean.
        .route(
            "/torrent-settings",
            get(routes::get_torrent_settings).post(routes::post_torrent_settings),
        )
        .route("/network-info", get(routes::network_info))
        .route("/device-info", get(routes::device_info))
        .route("/casting", get(routes::casting))
        .route("/casting/", get(routes::casting))
        .route("/heartbeat", get(routes::heartbeat))
        .route("/", get(routes::root))
        .route("/favicon.ico", get(routes::favicon))
        // M1 torrent engine. POST-only: these mutate state, so they must not be
        // reachable from a foreign page's `<img src>` / navigation (a GET with no
        // Origin). The web client + core already POST create; nothing issues remove.
        .route("/create", post(torrent::create_blob))
        .route("/{info_hash}/create", post(torrent::create_magnet))
        .route("/removeAll", post(torrent::remove_all))
        .route("/{info_hash}/remove", post(torrent::remove))
        // Cache management (Rillio-specific): the Cached page + per-stream
        // Download buttons. Reads GET, mutations POST-only (see cache_api).
        .route("/cache/list", get(cache_api::list))
        .route("/cache/download", post(cache_api::download))
        .route("/cache/pin", post(cache_api::pin))
        .route("/cache/pause", post(cache_api::pause))
        .route("/cache/watched", post(cache_api::watched))
        // The file browser: what else is inside a cached torrent, and fetching it.
        .route("/cache/files/{info_hash}", get(cache_api::files))
        .route("/cache/select", post(cache_api::select))
        // What media a cached torrent actually is (addon metadata sidecar).
        .route("/cache/meta", post(cache_api::meta))
        .route("/cache/delete", post(cache_api::delete))
        // "Fetch again" for an entry with an issue (see cache_api::Issue).
        .route("/cache/refetch", post(cache_api::refetch))
        // M2 stats family (static segments; win over the {idx} stream param).
        .route("/stats.json", get(stats::stats_aggregate))
        .route("/{info_hash}/stats.json", get(stats::stats_torrent))
        .route("/{info_hash}/{idx}/stats.json", get(stats::stats_file))
        // M3a header-injecting media proxy + HLS playlist rewriter (all methods).
        .route("/proxy/{opts}", any(proxy::proxy_root))
        .route("/proxy/{opts}/{*path}", any(proxy::proxy_with_path))
        // M3b support routes.
        .route("/opensubHash", get(support::opensub_hash))
        .route("/subtitles.vtt", get(support::subtitles_vtt))
        .route("/subtitles.srt", get(support::subtitles_srt))
        .route("/subtitlesTracks", get(support::subtitles_tracks))
        .route("/tracks/{url}", get(support::tracks))
        .route("/yt/{id}", get(support::yt))
        // /hlsv2/probe - report direct-playable so the player uses the direct
        // stream URL (mpv shell: no server transcode). Rest of /hlsv2 deferred.
        .route("/hlsv2/probe", get(hlsv2::probe))
        // M4 local-files addon transport (manifest so core recognizes it;
        // resources return empty - full indexing deferred).
        .route("/local-addon/manifest.json", get(local_addon::local_manifest))
        .route("/local-addon/{resource}/{type}/{*rest}", get(local_addon::local_resource))
        // The media stream. GET+HEAD are handled explicitly (HEAD must not open
        // the FileStream), so we register both methods on one handler rather
        // than let axum synthesize HEAD from GET.
        .route(
            "/{info_hash}/{idx}",
            on(MethodFilter::GET.or(MethodFilter::HEAD), stream::stream),
        )
        .route(
            "/{info_hash}/{idx}/{*rest}",
            on(MethodFilter::GET.or(MethodFilter::HEAD), stream::stream_rest),
        )
        // Innermost: request logging. Middle: CORS (echoes ACAO for the allowlisted
        // webview + answers its Private Network Access preflight). Outermost: the
        // origin guard, so a foreign Origin is rejected before CORS or any handler.
        .layer(axum::middleware::from_fn(log_request))
        .layer(security::cors_layer())
        .layer(axum::middleware::from_fn(security::origin_guard))
        .with_state(state)
}

/// Log every incoming request (method + path + Origin). Diagnostic; cheap enough
/// to keep. The Origin is logged so the trusted webview origin can be confirmed
/// against the allowlist in [`security`] without guessing.
async fn log_request(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let origin = req
        .headers()
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>")
        .to_owned();
    tracing::debug!("REQ {method} {uri} Origin={origin}");
    next.run(req).await
}

/// Bind the socket FIRST, then build everything else around the port we actually
/// got. Returns the bound address, the [`Engine`], a clone of the [`Router`] (so
/// an embedder can dispatch requests in-process without a socket) and the future
/// that serves the listener until it errors.
///
/// The order matters. `config.bind` is only a *request*: [`bind_with_fallback`]
/// may hand back an ephemeral port instead, and two things downstream are derived
/// from the config rather than from the listener:
///
/// - `config.base_url` - the only absolute URL the server advertises
///   (`/settings.baseUrl`, which the web client uses as a "server is up" gate and
///   hands to cast devices).
/// - `config.bind.port()` - the SSRF self-port exception in `support.rs`
///   (`Policy::AllowSelf`), which is what lets `/opensubHash` and the subtitle
///   routes re-fetch a URL that points back at our own loopback socket.
///
/// Both are patched from `local_addr()` BEFORE [`router`] is called, so a
/// fallback port propagates everywhere instead of leaving the server advertising
/// (and self-allowing) a port nothing is listening on. `base_url`'s host is left
/// alone - it is deliberately independent of `bind` (behind a container the two
/// differ) - only its port follows the socket.
///
/// The two sweepers are spawned exactly as [`serve`] used to spawn them; they
/// need a tokio runtime, so call this inside one.
pub async fn bind_and_serve(
    mut config: Config,
) -> std::io::Result<(SocketAddr, Engine, Router, impl Future<Output = std::io::Result<()>>)> {
    let listener = bind_with_fallback(config.bind).await?;
    let addr = listener.local_addr()?;

    config.bind = addr;
    if config.base_url.set_port(Some(addr.port())).is_err() {
        // Cannot-be-a-base URL (mailto:, data:, ...). Fail loud rather than
        // advertise a base_url pointing at the wrong port.
        return Err(std::io::Error::other(format!(
            "base_url {} cannot carry a port",
            config.base_url
        )));
    }

    // Serve the socket from NOW, before the engine exists. The listener is
    // bound, so the kernel is already accepting connections into its backlog;
    // with nothing serving them until `Engine::new` returned, a slow or stuck
    // engine start left every request hanging with zero bytes until the
    // client gave up (measured 2026-10-12: 12 to 25 s per request, for
    // minutes). The gate answers 503 with the reason until the real router is
    // in place, then forwards to it.
    let slot: RouterSlot = Arc::new(OnceLock::new());
    let serving = tokio::spawn({
        let gate = gate_router(slot.clone());
        async move { axum::serve(listener, gate).await }
    });

    let engine = match Engine::new(config.cache_root.clone()).await {
        Ok(engine) => engine,
        Err(e) => {
            // Stop serving and free the port, so a retry can bind it again.
            serving.abort();
            let _ = serving.await;
            return Err(std::io::Error::other(format!("{e:#}")));
        }
    };
    spawn_cache_sweeper(&config, engine.clone());
    spawn_ephemeral_sweeper(engine.clone());
    let app = router(config, engine.clone());
    slot.set(app.clone()).expect("the router slot is filled exactly once");
    tracing::info!(%addr, "streaming server listening");

    let fut = async move {
        match serving.await {
            Ok(served) => served,
            Err(e) => Err(std::io::Error::other(format!("the HTTP serve task ended: {e}"))),
        }
    };
    Ok((addr, engine, app, fut))
}

/// Where the real router goes once the engine exists (see [`gate_router`]).
type RouterSlot = Arc<OnceLock<Router>>;

/// What the socket serves from the moment it is bound: every request is
/// forwarded to the real router once it is in `slot`, and until then answered
/// `503` with a JSON reason, so a client always gets an answer and can tell
/// "still starting" from "not there". The real router's own layers (origin
/// guard, CORS) apply to everything forwarded; the 503 carries no state.
fn gate_router(slot: RouterSlot) -> Router {
    Router::new().fallback(move |req: axum::extract::Request| {
        let slot = slot.clone();
        async move {
            use axum::response::IntoResponse;
            use tower::ServiceExt;
            match slot.get() {
                Some(app) => match app.clone().oneshot(req).await {
                    Ok(response) => response,
                    Err(never) => match never {},
                },
                None => (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    [(axum::http::header::RETRY_AFTER, "2")],
                    axum::Json(serde_json::json!({
                        "error": "the streaming server is starting (restoring the torrent session)"
                    })),
                )
                    .into_response(),
            }
        }
    })
}

#[cfg(test)]
mod gate_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    async fn get(app: &Router, path: &str) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// Before the engine exists every request is answered at once, with the
    /// reason; afterwards the very same service is the real router.
    #[tokio::test]
    async fn the_socket_answers_before_the_engine_exists_and_forwards_after() {
        let slot: RouterSlot = Arc::new(OnceLock::new());
        let gate = gate_router(slot.clone());

        let (status, body) = get(&gate, "/cache/list").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("starting"), "the 503 says why: {body}");

        let dir = std::env::temp_dir().join("rillio-gate-router");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let engine = Engine::new(dir.clone()).await.unwrap();
        slot.set(router(Config::local(dir), engine)).unwrap();

        let (status, body) = get(&gate, "/cache/list").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "[]");
        let (status, _) = get(&gate, "/no-such-route").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "the real router's own 404");
    }
}

/// Bind `requested`, falling back to an ephemeral port on the same IP when the
/// port itself is refused.
///
/// Two refusals mean "this port, not this machine": `AddrInUse` (something else
/// holds it) and `PermissionDenied` - on Windows a port inside a reserved
/// exclusion range (`netsh interface ipv4 show excludedportrange`, Hyper-V and
/// friends reserve blocks at boot) fails with WSAEACCES 10013, which Rust maps to
/// `PermissionDenied`, NOT to `AddrInUse`. Anything else (a bad interface, for
/// instance) is a real error and propagates.
///
/// Both the refusal and the port we settled on are logged at warn level: a
/// fallback boot changes the URL of every external-player deep link, so it must
/// be obvious in the log why.
async fn bind_with_fallback(requested: SocketAddr) -> std::io::Result<tokio::net::TcpListener> {
    let err = match tokio::net::TcpListener::bind(requested).await {
        Ok(listener) => return Ok(listener),
        Err(e) if is_port_refusal(&e) => e,
        Err(e) => return Err(e),
    };
    tracing::warn!(
        requested = %requested,
        kind = ?err.kind(),
        error = %err,
        "streaming server port refused; retrying on an ephemeral port"
    );
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(requested.ip(), 0)).await?;
    let addr = listener.local_addr()?;
    tracing::warn!(
        requested = %requested,
        fallback = %addr,
        "streaming server fell back to an ephemeral port"
    );
    Ok(listener)
}

/// Is this bind error about the PORT (retry on :0) rather than the machine?
fn is_port_refusal(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
    )
}

/// Bind `config.bind` and serve until the process is signalled. Builds the
/// engine from `config.cache_root`. Embedders that need the port, the engine or
/// the router call [`bind_and_serve`] instead; this is the standalone bin's
/// convenience wrapper over it.
pub async fn serve(config: Config) -> std::io::Result<()> {
    let (_addr, _engine, _router, fut) = bind_and_serve(config).await?;
    fut.await
}

/// How often the cache sweeper checks disk usage.
const CACHE_SWEEP_INTERVAL: Duration = Duration::from_secs(30);
/// A torrent touched (streamed/queried) within this window is never evicted, so
/// whatever is currently playing is protected from the cache cap.
const CACHE_EVICT_GRACE: Duration = Duration::from_secs(120);

/// How long after the player marks a stream watched before streaming mode may
/// delete it. Long enough that "rewatch that scene" and finishing an episode
/// while the next one plays both survive; the mark also persists across
/// restarts, so a missed window just cleans up on a later sweep.
const EPHEMERAL_TTL: Duration = Duration::from_secs(60 * 60);
/// How often the ephemeral sweeper looks for watched streams to clean up.
const EPHEMERAL_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Streaming mode's cleanup loop: delete watched, un-kept streams once their
/// watched mark is [`EPHEMERAL_TTL`] old. Always spawned (unlike the cache-cap
/// sweeper, which needs a configured cap) - whether it acts is decided per
/// sweep by the persisted `streaming_mode` toggle, so flipping the setting in
/// the UI applies without a restart. Pinned torrents and anything recently
/// streamed are never touched (see [`Engine::sweep_watched`]).
fn spawn_ephemeral_sweeper(engine: Engine) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(EPHEMERAL_SWEEP_INTERVAL);
        loop {
            tick.tick().await;
            engine.sweep_watched(EPHEMERAL_TTL, CACHE_EVICT_GRACE).await;
        }
    });
}

/// Enforce `config.cache_size` (S7): periodically evict the least-recently-used
/// idle torrents when the cache exceeds the cap. `None` cacheSize = unlimited (no
/// sweeper). A streaming server plays a *window* of a torrent, so adds are never
/// refused by size (see `tests/confinement.rs`); the bound is applied here, after
/// the fact, by eviction rather than refusal.
fn spawn_cache_sweeper(config: &Config, engine: Engine) {
    let Some(cap) = config.cache_size else {
        tracing::info!("cache-cap: unlimited (no cacheSize); disk growth is unbounded");
        return;
    };
    let cap = cap.max(0.0) as u64;
    tracing::info!("cache-cap: enforcing ~{cap} bytes by evicting idle torrents");
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(CACHE_SWEEP_INTERVAL);
        loop {
            tick.tick().await;
            engine.enforce_cache_cap(cap, CACHE_EVICT_GRACE).await;
        }
    });
}
