use std::net::SocketAddr;
use std::path::PathBuf;

use url::Url;

/// Runtime configuration for the streaming server.
///
/// `base_url` is what the server advertises to clients (it appears in
/// `/settings.baseUrl` and the `remoteHttps` option list). It is distinct from
/// `bind`, which is only where the socket listens - behind the container these
/// differed (bind `0.0.0.0:11470`, advertised the bridge IP).
#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub base_url: Url,
    pub app_path: PathBuf,
    pub cache_root: PathBuf,
    /// Cache size in bytes. `None` = unlimited (the "∞" selection).
    pub cache_size: Option<f64>,
    pub server_version: String,
    /// `/proxy` SSRF allowlist: exact hosts permitted to resolve to private /
    /// loopback / link-local ranges. Empty = block all such destinations.
    pub proxy_allow_private_hosts: Vec<String>,
}

/// Default streaming-server port, matching `STREAMING_SERVER_URL` in
/// `crates/core/src/constants.rs` (`http://127.0.0.1:11470`).
pub const DEFAULT_PORT: u16 = 11470;

/// Reported to clients as `serverVersion`. This identifies the Rust
/// implementation; core only displays it and does not gate on a value.
pub const SERVER_VERSION: &str = concat!("5.0.0-rust+", env!("CARGO_PKG_VERSION"));

/// The address the server is NAMED by before anyone knows where it bound: the
/// core builds every stream url from it, and a boot that fell back to another
/// port (see `bind_with_fallback`) still sees urls on this origin. Whoever
/// knows the real address maps this one onto it.
pub fn default_base_url() -> Url {
    Url::parse(&format!("http://127.0.0.1:{DEFAULT_PORT}")).expect("hardcoded base url is valid")
}

impl Config {
    /// Loopback config on the default port, cache under `app_path`.
    pub fn local(app_path: PathBuf) -> Self {
        let bind = SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT));
        let base_url = default_base_url();
        Self {
            bind,
            base_url,
            cache_root: app_path.clone(),
            app_path,
            // Unlimited by product decision (the cache is transient and the user
            // was never warned before content is deleted). The eviction machinery
            // exists and is enforced whenever a FINITE cacheSize is configured
            // (see `serve`/`Engine::enforce_cache_cap`), but the default must not
            // silently delete downloaded content. `None` => `/settings` reports the
            // "∞" selection. Wiring the user's Settings choice through to a finite
            // cap here is the follow-up.
            cache_size: None,
            server_version: SERVER_VERSION.to_owned(),
            proxy_allow_private_hosts: Vec::new(),
        }
    }
}
