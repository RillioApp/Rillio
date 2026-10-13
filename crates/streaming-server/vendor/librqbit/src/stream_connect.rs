#[cfg(not(windows))]
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

/// How many dials may be in flight on the blocking pool at once: half of it.
/// Each one occupies a pool thread (on Windows for the length of its
/// handshake, elsewhere for the length of its `connect()` call), and the pool
/// is also what file I/O and DNS run on, so dials never get more than a fixed
/// share of it. A dial that finds no slot waits for one inside its caller's
/// connect timeout.
const DIAL_SLOTS: usize = BLOCKING_POOL_THREADS / 2;

fn dial_slots() -> Arc<Semaphore> {
    static SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SLOTS.get_or_init(|| Arc::new(Semaphore::new(DIAL_SLOTS))).clone()
}

/// RILLIO (see vendor/README-RILLIO.md). An outgoing TCP connection whose
/// `connect()` is never issued from a runtime worker thread, and on Windows is
/// never the non-blocking `connect()` call at all.
///
/// tokio issues that call on the first poll of its connect future, on
/// whichever worker polls it, trusting a non-blocking `connect()` to return at
/// once. A kernel socket filter can break that promise: with AdGuard's socket
/// driver the call was held for the whole TCP handshake, 20 s for an address
/// nobody answers (measured 2026-10-13: 20,007 ms then `WSAEWOULDBLOCK`,
/// 0.1 ms with the filter off). A live torrent dials up to 128 peers, so every
/// worker of the runtime ended up parked inside the kernel and nothing else on
/// it ran. Worse, a thread inside that call cannot be got out of it:
/// `CancelSynchronousIo`, `CancelIoEx` and closing the socket from another
/// thread all left it held its full 20 s, and a process with such threads does
/// not finish exiting until they return (8 held dials: gone 20,088 ms after
/// `ExitProcess`).
///
/// The same filter does NOT hold an overlapped `ConnectEx` (measured the same
/// day: 1.2 ms to start a dial, a process with 16 in flight exits at once).
/// So on Windows the dial is a `ConnectEx`, awaited on a blocking-pool thread
/// in an ordinary wait, which the caller's timeout can end. Elsewhere the dial
/// is tokio's own, with the poll that issues `connect()` moved to the blocking
/// pool.
async fn dial_off_the_workers(addr: SocketAddr) -> std::io::Result<tokio::net::TcpStream> {
    let slot = dial_slots()
        .acquire_owned()
        .await
        .map_err(|_| std::io::Error::other("the dial slots are closed"))?;
    dial_with_slot(addr, slot).await
}

#[cfg(windows)]
async fn dial_with_slot(
    addr: SocketAddr,
    slot: tokio::sync::OwnedSemaphorePermit,
) -> std::io::Result<tokio::net::TcpStream> {
    let abandoned = Arc::new(overlapped::Event::new()?);
    // Dropped when this future is (the caller's connect timeout, a cancelled
    // peer task): that is what ends the wait on the pool thread.
    let _give_up = overlapped::SetOnDrop(abandoned.clone());
    let connected = tokio::task::spawn_blocking(move || {
        // The slot counts the pool thread, so it is released by the thread.
        let _slot = slot;
        test_hooks::hold_like_a_filtered_connect();
        overlapped::connect(addr, &abandoned)
    })
    .await
    .map_err(|e| std::io::Error::other(format!("the dial's blocking task failed: {e}")))??;
    tokio::net::TcpStream::from_std(connected)
}

/// `ConnectEx` with an event to wait on. Windows only.
#[cfg(windows)]
mod overlapped {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::os::windows::io::AsRawSocket;
    use std::sync::Arc;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::Networking::WinSock::{
        setsockopt, WSAGetLastError, WSAGetOverlappedResult, WSAIoctl, LPFN_CONNECTEX,
        SIO_GET_EXTENSION_FUNCTION_POINTER, SOCKADDR, SOCKET, SOL_SOCKET, SO_UPDATE_CONNECT_CONTEXT,
        WSAID_CONNECTEX, WSA_IO_PENDING,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, INFINITE};
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    /// A manual-reset event, closed on drop.
    pub(super) struct Event(HANDLE);

    // SAFETY: an event handle may be set, waited on and closed from any thread.
    unsafe impl Send for Event {}
    unsafe impl Sync for Event {}

    impl Event {
        pub(super) fn new() -> std::io::Result<Self> {
            // SAFETY: no name, no security attributes; manual reset, not signalled.
            let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self(handle))
        }
    }

    impl Drop for Event {
        fn drop(&mut self) {
            // SAFETY: the handle is ours and is closed exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Signals the event when dropped.
    pub(super) struct SetOnDrop(pub(super) Arc<Event>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            // SAFETY: the Arc keeps the handle open.
            unsafe { SetEvent(self.0 .0) };
        }
    }

    fn last_wsa_error() -> std::io::Error {
        // SAFETY: reads the calling thread's last WinSock error.
        std::io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    }

    /// The `ConnectEx` entry point, which WinSock only hands out at run time.
    fn connect_ex(socket: SOCKET) -> std::io::Result<LPFN_CONNECTEX> {
        let guid = WSAID_CONNECTEX;
        let mut function: LPFN_CONNECTEX = None;
        let mut returned = 0u32;
        // SAFETY: in and out buffers are the sizes passed; no overlapped call.
        let failed = unsafe {
            WSAIoctl(
                socket,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                &guid as *const _ as *const _,
                std::mem::size_of_val(&guid) as u32,
                &mut function as *mut _ as *mut _,
                std::mem::size_of_val(&function) as u32,
                &mut returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if failed != 0 {
            return Err(last_wsa_error());
        }
        Ok(function)
    }

    /// Connect to `addr`, or stop trying when `abandoned` is signalled. Blocks
    /// the calling thread in an ordinary wait. The returned stream is
    /// non-blocking, ready for `tokio::net::TcpStream::from_std`.
    pub(super) fn connect(addr: SocketAddr, abandoned: &Event) -> std::io::Result<std::net::TcpStream> {
        let (domain, any): (_, SocketAddr) = match addr {
            SocketAddr::V4(_) => (socket2::Domain::IPV4, (Ipv4Addr::UNSPECIFIED, 0).into()),
            SocketAddr::V6(_) => (socket2::Domain::IPV6, (Ipv6Addr::UNSPECIFIED, 0).into()),
        };
        let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
        // ConnectEx requires a bound socket.
        socket.bind(&any.into())?;
        let raw = socket.as_raw_socket() as SOCKET;
        let Some(connect_ex) = connect_ex(raw)? else {
            return Err(std::io::Error::other("WinSock handed out no ConnectEx"));
        };

        let done = Event::new()?;
        // SAFETY: OVERLAPPED is plain data; all zeroes is its documented initial state.
        let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
        operation.hEvent = done.0;
        let target = socket2::SockAddr::from(addr);
        // SAFETY: `target` and `operation` live until the operation has
        // finished: every path below waits for it before returning.
        let started = unsafe {
            connect_ex(
                raw,
                target.as_ptr() as *const SOCKADDR,
                target.len(),
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                &mut operation,
            )
        };
        if started == 0 {
            // SAFETY: as in `last_wsa_error`.
            let error = unsafe { WSAGetLastError() };
            if error != WSA_IO_PENDING {
                return Err(std::io::Error::from_raw_os_error(error));
            }
            let handles = [done.0, abandoned.0];
            // SAFETY: two valid handles, both kept open by their owners here.
            let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
            if woke != WAIT_OBJECT_0 {
                // The caller gave up (or the wait itself failed): cancel, then
                // wait until the operation has let go of `operation`.
                let mut ignored = 0u32;
                // SAFETY: the socket and `operation` are alive; the wait makes
                // sure the kernel is done with `operation` before it is freed.
                unsafe {
                    CancelIoEx(raw as HANDLE, &operation);
                    GetOverlappedResult(raw as HANDLE, &operation, &mut ignored, 1);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the dial was abandoned before the peer answered",
                ));
            }
        }
        let (mut transferred, mut flags) = (0u32, 0u32);
        // SAFETY: the operation has completed; this only reads its result.
        if unsafe { WSAGetOverlappedResult(raw, &operation, &mut transferred, 0, &mut flags) } == 0 {
            return Err(last_wsa_error());
        }
        // Without this the socket is "connected" for send/recv only:
        // getpeername, shutdown and friends fail on it.
        // SAFETY: no option value is passed.
        if unsafe { setsockopt(raw, SOL_SOCKET, SO_UPDATE_CONNECT_CONTEXT, std::ptr::null(), 0) } != 0 {
            return Err(last_wsa_error());
        }
        socket.set_nonblocking(true)?;
        Ok(socket.into())
    }
}

/// Not Windows: tokio's own connect, with its FIRST poll, the one that issues
/// `connect()`, run on the blocking pool; the future is then handed back and
/// awaited on the worker as usual. Polling it once with a waker nobody listens
/// to is sound: the next poll (the `.await` below) registers the real one.
#[cfg(not(windows))]
async fn dial_with_slot(
    addr: SocketAddr,
    slot: tokio::sync::OwnedSemaphorePermit,
) -> std::io::Result<tokio::net::TcpStream> {
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
