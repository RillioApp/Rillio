use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};

use anyhow::Context;
use tokio::sync::Semaphore;

#[derive(Debug, Clone)]
pub(crate) struct SocksProxyConfig {
    pub host: String,
    pub port: u16,
    pub username_password: Option<(String, String)>,
}

impl SocksProxyConfig {
    pub fn parse(url: &str) -> anyhow::Result<Self> {
        let url = ::url::Url::parse(url).context("invalid proxy URL")?;
        if url.scheme() != "socks5" {
            anyhow::bail!("proxy URL should have socks5 scheme");
        }
        let host = url.host_str().context("missing host")?;
        let port = url.port().context("missing port")?;
        let up = url
            .password()
            .map(|p| (url.username().to_owned(), p.to_owned()));
        Ok(Self {
            host: host.to_owned(),
            port,
            username_password: up,
        })
    }

    async fn connect(
        &self,
        addr: SocketAddr,
    ) -> anyhow::Result<(
        impl tokio::io::AsyncRead + Unpin,
        impl tokio::io::AsyncWrite + Unpin,
    )> {
        let proxy_addr = (self.host.as_str(), self.port);

        let stream = if let Some((username, password)) = self.username_password.as_ref() {
            tokio_socks::tcp::Socks5Stream::connect_with_password(
                proxy_addr,
                addr,
                username.as_str(),
                password.as_str(),
            )
            .await
            .context("error connecting to proxy")?
        } else {
            tokio_socks::tcp::Socks5Stream::connect(proxy_addr, addr)
                .await
                .context("error connecting to proxy")?
        };

        Ok(tokio::io::split(stream))
    }
}

#[derive(Debug, Default)]
pub(crate) struct StreamConnector {
    proxy_config: Option<SocksProxyConfig>,
}

impl From<Option<SocksProxyConfig>> for StreamConnector {
    fn from(proxy_config: Option<SocksProxyConfig>) -> Self {
        Self { proxy_config }
    }
}

impl StreamConnector {
    pub async fn connect(
        &self,
        addr: SocketAddr,
    ) -> anyhow::Result<(
        Box<dyn tokio::io::AsyncRead + Send + Unpin>,
        Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    )> {
        if let Some(proxy) = self.proxy_config.as_ref() {
            let (r, w) = proxy.connect(addr).await?;
            return Ok((Box::new(r), Box::new(w)));
        }

        let (r, w) = dial_off_the_workers(addr)
            .await
            .context("error connecting")?
            .into_split();
        Ok((Box::new(r), Box::new(w)))
    }
}

/// tokio's default cap on blocking-pool threads
/// (`runtime::Builder::max_blocking_threads`). The runtime's real cap cannot be
/// read from inside it; every runtime this crate is run on in Rillio keeps the
/// default.
const BLOCKING_POOL_THREADS: usize = 512;

/// How many dials may have their `connect()` call in flight at once: a quarter
/// of the blocking pool. Each one occupies a pool thread for as long as the
/// call is held (nothing on a healthy machine, the whole handshake under a
/// filter), and the pool is also what file I/O and DNS run on, so dials never
/// get more than a fixed share of it. A dial that finds no slot waits for one
/// inside its caller's connect timeout.
const DIAL_SLOTS: usize = BLOCKING_POOL_THREADS / 4;

fn dial_slots() -> Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(DIAL_SLOTS))).clone()
}

/// RILLIO (see vendor/README-RILLIO.md). `TcpStream::connect`, except that the
/// `connect()` system call is not issued from a runtime worker thread.
///
/// tokio issues the call on the first poll of the connect future, on whichever
/// worker polls it, trusting a non-blocking `connect()` to return at once. A
/// kernel socket filter can break that promise: with AdGuard's socket driver
/// the call was held for the whole TCP handshake, 20 s for an address nobody
/// answers (measured 2026-10-13: 20,007 ms then `WSAEWOULDBLOCK`, 0.1 ms with
/// the filter off). A live torrent dials up to 128 peers, so every worker of
/// the runtime ended up parked inside the kernel and nothing else on it ran.
///
/// So the FIRST poll, the one that issues the call, runs on the blocking pool,
/// and the future is then handed back and awaited here as usual. Polling it
/// once with a waker nobody listens to is sound: the next poll (the `.await`
/// below) registers the real one. Nothing else about connecting changes.
async fn dial_off_the_workers(addr: SocketAddr) -> std::io::Result<tokio::net::TcpStream> {
    let slot = dial_slots()
        .acquire_owned()
        .await
        .map_err(|_| std::io::Error::other("the dial slots are closed"))?;
    let runtime = tokio::runtime::Handle::current();
    let mut connecting = Box::pin(tokio::net::TcpStream::connect(addr));
    let (connecting, first) = tokio::task::spawn_blocking(move || {
        // The slot is held until the system call has returned, also when the
        // caller gave up waiting (its timeout dropped the join handle): the
        // pool thread is what the slot counts.
        let _slot = slot;
        let _in_runtime = runtime.enter();
        test_hooks::hold_like_a_filtered_connect();
        let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
        let first = connecting.as_mut().poll(&mut cx);
        (connecting, first)
    })
    .await
    .map_err(|e| std::io::Error::other(format!("the dial's blocking task failed: {e}")))?;
    match first {
        std::task::Poll::Ready(connected) => connected,
        std::task::Poll::Pending => connecting.await,
    }
}

/// RILLIO (see vendor/README-RILLIO.md): a way for a test to model a kernel
/// socket filter that holds the non-blocking `connect()` call.
#[doc(hidden)]
pub mod test_hooks {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static HOLD_CONNECT_MS: AtomicU64 = AtomicU64::new(0);

    /// From now on every outgoing dial sleeps this long, synchronously, on the
    /// thread that issues the `connect()` call, before issuing it. Zero (the
    /// default, and the only value outside tests) is no hold at all.
    pub fn hold_outgoing_connects(hold: Duration) {
        HOLD_CONNECT_MS.store(hold.as_millis() as u64, Ordering::SeqCst);
    }

    pub(crate) fn hold_like_a_filtered_connect() {
        let ms = HOLD_CONNECT_MS.load(Ordering::Relaxed);
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
}
