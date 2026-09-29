//! Render an error WITH its cause chain.
//!
//! An error's `Display` is only its top layer. reqwest's is "error sending
//! request for url (...)" and nothing more: the part that says what actually
//! went wrong (DNS failure, TLS handshake, connection reset, timeout) lives in
//! `source()`, and `to_string()` drops it. The v0.1.44 -> v0.1.45 update
//! failure (2026-09-29) reached the screen as exactly that top line, which told
//! neither the user nor us anything. Every error the shell shows or logs for a
//! network or update failure goes through [`error_chain`].
//!
//! The screen never shows the chain: it says what happened and what to check
//! ([`classify`], [`wording`], carried by [`UpdateFailure`]), and "Copy error"
//! puts the whole technical report on the clipboard. Android has no in-app
//! updater: only the platform refusal is built there.
#![cfg_attr(target_os = "android", allow(dead_code))]

/// `err` followed by every `source()` below it, joined with ": ", e.g.
/// "error sending request for url (https://...): client error (Connect): tcp
/// connect error: No connection could be made ... (os error 10061)".
///
/// A layer whose text is already part of the message is skipped: some errors
/// print their own cause in `Display` (and `#[error(transparent)]` wrappers
/// forward it), and repeating it would only bury the real cause in noise.
pub fn error_chain<E: std::error::Error + ?Sized>(err: &E) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    // A cap, not a feature: a (buggy) cyclic chain must not hang the caller.
    let mut depth = 0;
    while let Some(cause) = source {
        depth += 1;
        if depth > 32 {
            out.push_str(": ...");
            break;
        }
        let text = cause.to_string();
        if !text.is_empty() && !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = cause.source();
    }
    out
}

/// What went wrong, as far as the chain PROVES it. The screen leads with a
/// plain sentence for this (see [`summary`]); the chain stays one click away.
///
/// Classified here, on typed errors, rather than by matching text in the page:
/// the shell can see `io::ErrorKind`, raw Windows socket codes and rustls'
/// error type, the page only sees prose. Unknown is [`FailureKind::Other`],
/// never a guess.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureKind {
    Reset,
    Dns,
    Refused,
    Timeout,
    Tls,
    Other,
}

/// Which step of an update failed. Serialized (kebab-case) as the journal's
/// `stage=` label and sent to the page, which words its footer from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// Settings -> "Check for updates".
    Check,
    /// The re-check `install_update` runs before downloading.
    InstallCheck,
    Download,
    /// Handing the downloaded installer to the OS, or the installer itself.
    Install,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Check => "check",
            Stage::InstallCheck => "install-check",
            Stage::Download => "download",
            Stage::Install => "install",
        }
    }
}

/// The classification table: the innermost layer that says something wins,
/// because the innermost layer is the cause.
pub fn classify(err: &(dyn std::error::Error + 'static)) -> FailureKind {
    let mut found = None;
    let mut layer = Some(err);
    let mut depth = 0;
    while let Some(cause) = layer {
        depth += 1;
        if depth > 32 {
            break;
        }
        if let Some(kind) = classify_layer(cause) {
            found = Some(kind);
        }
        layer = cause.source();
    }
    found.unwrap_or(FailureKind::Other)
}

/// One layer, by type. Only facts the error itself carries:
/// - `io::Error`: the raw Windows socket code first (WSAECONNRESET 10054,
///   WSAECONNABORTED 10053, WSAHOST_NOT_FOUND 11001 and its getaddrinfo
///   siblings 11002-11004, WSAECONNREFUSED 10061, WSAETIMEDOUT 10060), then
///   the portable `ErrorKind`. An io::Error that wraps a rustls error (how
///   tokio-rustls reports a failed handshake) is TLS. The inner error is
///   checked through `get_ref`, because io::Error's `source()` skips it.
/// - `rustls::Error` on its own layer: TLS.
/// - reqwest's own timeout is a private type (`reqwest::error::TimedOut`),
///   the one layer that can only be recognised by its exact text.
fn classify_layer(err: &(dyn std::error::Error + 'static)) -> Option<FailureKind> {
    #[cfg(not(target_os = "android"))]
    if err.downcast_ref::<rustls::Error>().is_some() {
        return Some(FailureKind::Tls);
    }
    if let Some(io) = err.downcast_ref::<std::io::Error>() {
        // io::Errors nest (the updater's TLS failure arrives as
        // io(Other, io(InvalidData, rustls::Error)), probe `tls_failure`), and
        // io::Error's `source()` skips the wrapped error, so descend through
        // `get_ref` by hand.
        let mut stack = vec![io];
        while let Some(inner) = stack.last().and_then(|io| io.get_ref()) {
            #[cfg(not(target_os = "android"))]
            if inner.downcast_ref::<rustls::Error>().is_some() {
                return Some(FailureKind::Tls);
            }
            match inner.downcast_ref::<std::io::Error>() {
                Some(nested) if stack.len() < 8 => stack.push(nested),
                _ => break,
            }
        }
        // Innermost first: it is the cause.
        return stack.iter().rev().find_map(|io| classify_io(io));
    }
    if err.to_string() == "operation timed out" {
        return Some(FailureKind::Timeout);
    }
    None
}

fn classify_io(io: &std::io::Error) -> Option<FailureKind> {
    match io.raw_os_error() {
        Some(10054) | Some(10053) => return Some(FailureKind::Reset),
        Some(11001..=11004) => return Some(FailureKind::Dns),
        Some(10061) => return Some(FailureKind::Refused),
        Some(10060) => return Some(FailureKind::Timeout),
        _ => {}
    }
    match io.kind() {
        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted => Some(FailureKind::Reset),
        std::io::ErrorKind::ConnectionRefused => Some(FailureKind::Refused),
        std::io::ErrorKind::TimedOut => Some(FailureKind::Timeout),
        _ => None,
    }
}

/// What the screen says for a failure: what happened, and what to check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wording {
    pub summary: &'static str,
    pub hint: &'static str,
    /// True where the failure provably changed nothing: a check or download
    /// that failed never reached the installer. The screen then adds
    /// [`NOTHING_INSTALLED`]. (Not "nothing on your device changed": the
    /// splash copy and progress file in %TEMP% are changes.)
    pub nothing_installed: bool,
}

pub const NOTHING_INSTALLED: &str = "Nothing was installed.";

/// THE table of what the screen says, per kind and step. Plain and true: a
/// kind the chain did not prove gets the generic line for its step; a check
/// never says "downloading"; an installer failure never gets a network line.
pub fn wording(kind: FailureKind, stage: Stage) -> Wording {
    let network = |summary, hint| Wording { summary, hint, nothing_installed: true };
    let checking = matches!(stage, Stage::Check | Stage::InstallCheck);
    match stage {
        Stage::Install => Wording {
            summary: "The update downloaded but couldn't be installed.",
            hint: "Try again, or download the latest version from rillio.app.",
            nothing_installed: false,
        },
        Stage::Check | Stage::InstallCheck | Stage::Download => match kind {
            FailureKind::Reset if checking => network(
                "The connection dropped while checking for the update.",
                "Check your internet connection or VPN, then try again.",
            ),
            FailureKind::Reset => network(
                "The connection dropped while downloading the update.",
                "Check your internet connection or VPN, then try again.",
            ),
            FailureKind::Dns => network(
                "Rillio couldn't reach the update server.",
                "Make sure you're online, then try again.",
            ),
            FailureKind::Refused => network(
                "The update server refused the connection.",
                "A firewall or VPN may be blocking it. Try again, or turn the VPN off for a moment.",
            ),
            FailureKind::Timeout => network(
                "The update server took too long to answer.",
                "Your connection may be slow. Try again in a moment.",
            ),
            FailureKind::Tls => network(
                "Rillio couldn't make a secure connection to the update server.",
                "Check your internet connection. Antivirus or VPN software that inspects traffic can cause this.",
            ),
            FailureKind::Other if checking => network(
                "The update check didn't finish.",
                "Try again. If it keeps failing, copy the error and send it to us.",
            ),
            FailureKind::Other => network(
                "The download didn't finish.",
                "Try again. If it keeps failing, copy the error and send it to us.",
            ),
        },
    }
}

/// One update failure as both the update window and the web layer get it:
/// what happened and what to check (from [`wording`]), the kind behind it,
/// the full chain, and the text "Copy error" puts on the clipboard.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct UpdateFailure {
    pub stage: Stage,
    pub kind: FailureKind,
    /// What happened, plus [`NOTHING_INSTALLED`] where that is true.
    pub summary: String,
    /// What to check.
    pub hint: String,
    /// The full cause chain (see [`error_chain`]).
    pub message: String,
    /// Everything "Copy error" copies: app version, time, step, kind, chain.
    pub report: String,
}

impl UpdateFailure {
    pub fn from_error(stage: Stage, err: &(dyn std::error::Error + 'static), version: &str) -> Self {
        Self::classified(stage, classify(err), error_chain(err), version)
    }

    /// A failure the table words (the kind is known, the message is ours).
    pub fn classified(stage: Stage, kind: FailureKind, message: impl Into<String>, version: &str) -> Self {
        let w = wording(kind, stage);
        let summary = if w.nothing_installed {
            format!("{} {NOTHING_INSTALLED}", w.summary)
        } else {
            w.summary.to_string()
        };
        Self::build(stage, kind, summary, w.hint.into(), message.into(), version)
    }

    /// A failure outside the table (nothing to install, no updater on this
    /// platform, Rillio not answering a retry).
    pub fn plain(stage: Stage, summary: &str, hint: &str, message: impl Into<String>, version: &str) -> Self {
        Self::build(stage, FailureKind::Other, summary.into(), hint.into(), message.into(), version)
    }

    fn build(stage: Stage, kind: FailureKind, summary: String, hint: String, message: String, version: &str) -> Self {
        let report = report(version, now_ms(), stage, kind, &message);
        UpdateFailure { stage, kind, summary, hint, message, report }
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The "Copy error" text: everything we need from a pasted report, nothing
/// the user has to explain.
pub fn report(version: &str, at_ms: u64, stage: Stage, kind: FailureKind, message: &str) -> String {
    let kind = serde_json::to_value(kind).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
    format!(
        "Rillio {version} update error\nTime: {}\nStep: {}\nKind: {kind}\nError: {message}",
        utc_timestamp(at_ms),
        stage.label()
    )
}

/// `YYYY-MM-DDTHH:MM:SSZ` from epoch milliseconds (civil-from-days, so no
/// date crate for one line of text).
pub fn utc_timestamp(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::error_chain;
    use std::fmt;

    /// A wrapper that, like reqwest's error, prints only its own layer and
    /// exposes the cause through `source()`.
    #[derive(Debug)]
    struct Wrapper {
        what: &'static str,
        cause: Box<dyn std::error::Error + Send + Sync + 'static>,
    }
    impl fmt::Display for Wrapper {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.what)
        }
    }
    impl std::error::Error for Wrapper {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&*self.cause)
        }
    }

    /// A wrapper that already prints its cause in `Display`.
    #[derive(Debug)]
    struct Verbose(std::io::Error);
    impl fmt::Display for Verbose {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "tcp connect error: {}", self.0)
        }
    }
    impl std::error::Error for Verbose {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn nested_source_is_included() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset by peer");
        let err = Wrapper { what: "error sending request for url (https://example.test/x.exe)", cause: Box::new(io) };
        // The bug: the top-level Display alone drops the cause.
        assert_eq!(err.to_string(), "error sending request for url (https://example.test/x.exe)");
        assert_eq!(
            error_chain(&err),
            "error sending request for url (https://example.test/x.exe): connection reset by peer"
        );
    }

    #[test]
    fn three_layers_in_order() {
        let io = std::io::Error::new(std::io::ErrorKind::TimedOut, "operation timed out");
        let mid = Wrapper { what: "client error (Connect)", cause: Box::new(io) };
        let top = Wrapper { what: "error sending request", cause: Box::new(mid) };
        assert_eq!(error_chain(&top), "error sending request: client error (Connect): operation timed out");
    }

    #[test]
    fn a_layer_already_in_the_message_is_not_repeated() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let top = Wrapper { what: "error sending request", cause: Box::new(Verbose(io)) };
        assert_eq!(error_chain(&top), "error sending request: tcp connect error: connection refused");
    }

    #[test]
    fn a_plain_error_is_its_display() {
        let io = std::io::Error::new(std::io::ErrorKind::Other, "disk full");
        assert_eq!(error_chain(&io), "disk full");
    }

    /// The updater plugin's error wraps reqwest with `#[error(transparent)]`,
    /// so what the shell formats is reqwest's own chain.
    #[cfg(not(target_os = "android"))]
    #[test]
    fn updater_error_carries_the_chain() {
        let reset = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset by peer");
        let tls = Wrapper { what: "tls handshake eof", cause: Box::new(reset) };
        let err = tauri_plugin_updater::Error::Io(std::io::Error::new(std::io::ErrorKind::Other, tls));
        assert_eq!(err.to_string(), "tls handshake eof");
        assert_eq!(error_chain(&err), "tls handshake eof: connection reset by peer");
    }

    use super::{classify, report, utc_timestamp, wording, FailureKind, Stage, UpdateFailure};

    fn wrapped(cause: impl std::error::Error + Send + Sync + 'static) -> Wrapper {
        Wrapper { what: "error sending request for url (https://example.test/x.exe)", cause: Box::new(cause) }
    }
    fn os(code: i32) -> Wrapper {
        wrapped(std::io::Error::from_raw_os_error(code))
    }
    fn io_kind(kind: std::io::ErrorKind) -> Wrapper {
        wrapped(std::io::Error::new(kind, "x"))
    }

    #[test]
    fn classifies_each_kind() {
        use std::io::ErrorKind as K;
        assert_eq!(classify(&os(10054)), FailureKind::Reset);
        assert_eq!(classify(&os(10053)), FailureKind::Reset);
        assert_eq!(classify(&io_kind(K::ConnectionReset)), FailureKind::Reset);
        assert_eq!(classify(&io_kind(K::ConnectionAborted)), FailureKind::Reset);
        assert_eq!(classify(&os(11001)), FailureKind::Dns);
        assert_eq!(classify(&os(11004)), FailureKind::Dns);
        assert_eq!(classify(&os(10061)), FailureKind::Refused);
        assert_eq!(classify(&io_kind(K::ConnectionRefused)), FailureKind::Refused);
        assert_eq!(classify(&os(10060)), FailureKind::Timeout);
        assert_eq!(classify(&io_kind(K::TimedOut)), FailureKind::Timeout);
        // reqwest's private TimedOut, recognised by its exact text.
        let timed_out = Wrapper { what: "operation timed out", cause: Box::new(std::fmt::Error) };
        assert_eq!(classify(&wrapped(timed_out)), FailureKind::Timeout);
        // tokio-rustls reports a failed handshake as io::Error(InvalidData, rustls::Error).
        let tls = std::io::Error::new(K::InvalidData, rustls::Error::General("bad record".into()));
        assert_eq!(classify(&wrapped(tls)), FailureKind::Tls);
        // ...and hyper-util wraps that in another io::Error (seen in the
        // tls_failure probe): io(Other, io(InvalidData, rustls::Error)).
        let inner = std::io::Error::new(K::InvalidData, rustls::Error::General("bad record".into()));
        assert_eq!(classify(&wrapped(std::io::Error::new(K::Other, inner))), FailureKind::Tls);
        let nested_reset = std::io::Error::new(K::Other, std::io::Error::from_raw_os_error(10054));
        assert_eq!(classify(&wrapped(nested_reset)), FailureKind::Reset);
        assert_eq!(classify(&wrapped(rustls::Error::General("bad cert".into()))), FailureKind::Tls);
    }

    #[test]
    fn unknown_is_other_never_a_guess() {
        assert_eq!(classify(&io_kind(std::io::ErrorKind::Other)), FailureKind::Other);
        assert_eq!(classify(&io_kind(std::io::ErrorKind::InvalidData)), FailureKind::Other);
        // hyper's "server closed early" names no socket error: not a reset.
        let early = Wrapper { what: "connection closed before message completed", cause: Box::new(std::fmt::Error) };
        assert_eq!(classify(&wrapped(early)), FailureKind::Other);
        assert_eq!(classify(&std::io::Error::other("disk full")), FailureKind::Other);
        assert_eq!(wording(FailureKind::Other, Stage::Download).summary, "The download didn't finish.");
    }

    #[test]
    fn the_innermost_cause_wins() {
        let outer_timeout = Wrapper { what: "operation timed out", cause: Box::new(std::io::Error::from_raw_os_error(10054)) };
        assert_eq!(classify(&wrapped(outer_timeout)), FailureKind::Reset);
    }

    fn says(kind: FailureKind, stage: Stage) -> (&'static str, &'static str, bool) {
        let w = wording(kind, stage);
        (w.summary, w.hint, w.nothing_installed)
    }

    #[test]
    fn the_agreed_sentences() {
        let d = Stage::Download;
        assert_eq!(says(FailureKind::Reset, d), ("The connection dropped while downloading the update.", "Check your internet connection or VPN, then try again.", true));
        assert_eq!(says(FailureKind::Dns, d), ("Rillio couldn't reach the update server.", "Make sure you're online, then try again.", true));
        assert_eq!(says(FailureKind::Refused, d), ("The update server refused the connection.", "A firewall or VPN may be blocking it. Try again, or turn the VPN off for a moment.", true));
        assert_eq!(says(FailureKind::Timeout, d), ("The update server took too long to answer.", "Your connection may be slow. Try again in a moment.", true));
        assert_eq!(says(FailureKind::Tls, d), ("Rillio couldn't make a secure connection to the update server.", "Check your internet connection. Antivirus or VPN software that inspects traffic can cause this.", true));
        assert_eq!(says(FailureKind::Other, d), ("The download didn't finish.", "Try again. If it keeps failing, copy the error and send it to us.", true));
        // A check never downloaded anything, so it never says "downloading".
        assert_eq!(says(FailureKind::Reset, Stage::Check).0, "The connection dropped while checking for the update.");
        assert_eq!(says(FailureKind::Other, Stage::InstallCheck).0, "The update check didn't finish.");
        assert_eq!(says(FailureKind::Dns, Stage::Check).0, "Rillio couldn't reach the update server.");
        // The installer is not the network: its own lines whatever the kind,
        // and no "nothing was installed" (the installer may have started).
        for kind in [FailureKind::Reset, FailureKind::Other, FailureKind::Tls] {
            assert_eq!(says(kind, Stage::Install), ("The update downloaded but couldn't be installed.", "Try again, or download the latest version from rillio.app.", false));
        }
    }

    #[test]
    fn the_failure_serializes_for_the_page_and_the_web() {
        let failure = UpdateFailure::from_error(Stage::Download, &os(10054), "0.1.45");
        let json = serde_json::to_value(&failure).unwrap();
        assert_eq!(json["stage"], "download");
        assert_eq!(json["kind"], "reset");
        assert_eq!(json["summary"], "The connection dropped while downloading the update. Nothing was installed.");
        assert_eq!(json["hint"], "Check your internet connection or VPN, then try again.");
        assert!(json["message"].as_str().unwrap().starts_with("error sending request for url (https://example.test/x.exe): "));
        let report = json["report"].as_str().unwrap();
        assert!(report.starts_with("Rillio 0.1.45 update error\nTime: 20"), "{report}");
        assert!(report.contains("\nStep: download\nKind: reset\nError: error sending request for url"), "{report}");
        assert!(report.ends_with("(os error 10054)"), "{report}");
        // An install failure does not claim nothing changed.
        let install = UpdateFailure::classified(Stage::Install, FailureKind::Other, "x", "0.1.45");
        assert_eq!(install.summary, "The update downloaded but couldn't be installed.");
    }

    #[test]
    fn utc_timestamps() {
        assert_eq!(utc_timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_timestamp(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(utc_timestamp(1_790_674_937_521), "2026-09-29T09:42:17Z");
        assert_eq!(report("0.1.45", 0, Stage::Install, FailureKind::Tls, "boom"), "Rillio 0.1.45 update error\nTime: 1970-01-01T00:00:00Z\nStep: install\nKind: tls\nError: boom");
    }

    /// Real network failures through reqwest, wrapped the way the updater
    /// plugin wraps them. Ignored by default (they touch the resolver and the
    /// loopback stack, which is flaky on the dev box); run with
    /// `cargo test error_chain -- --ignored --nocapture` to print the chains.
    #[cfg(not(target_os = "android"))]
    mod probes {
        use super::super::{classify, error_chain, FailureKind};

        fn fetch(url: &str) -> (String, FailureKind) {
            fetch_with(url, std::time::Duration::from_secs(20))
        }

        fn fetch_with(url: &str, timeout: std::time::Duration) -> (String, FailureKind) {
            // What tauri-plugin-updater's check() does before its first
            // request (its reqwest is built with `rustls-no-provider`).
            if rustls::crypto::CryptoProvider::get_default().is_none() {
                let _ = rustls::crypto::ring::default_provider().install_default();
            }
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                // reqwest 0.13, the updater's (see Cargo.toml dev-dependencies).
                let client = reqwest_updater::Client::builder()
                    .timeout(timeout)
                    .build()
                    .unwrap();
                let result = async { client.get(url).send().await?.bytes().await }.await;
                let err = tauri_plugin_updater::Error::Reqwest(result.expect_err("the request must fail"));
                let top = err.to_string();
                let chain = error_chain(&err);
                let kind = classify(&err);
                println!("top-level Display: {top}");
                println!("error_chain:       {chain}");
                println!("classify:          {kind:?}");
                assert!(chain.len() > top.len(), "the chain must add the cause");
                (chain, kind)
            })
        }

        #[test]
        #[ignore]
        fn dns_failure() {
            // `.invalid` never resolves (RFC 2606). Plain http: resolution
            // fails before any TLS would start.
            let (chain, kind) = fetch("http://rillio-update-probe.invalid/Rillio_x64-setup.exe");
            assert!(chain.contains("dns error"), "{chain}");
            assert_eq!(kind, FailureKind::Dns);
        }

        #[test]
        #[ignore]
        fn connection_refused() {
            // Bind then drop: the port is free and nothing listens on it.
            let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            let (chain, kind) = fetch(&format!("http://127.0.0.1:{port}/Rillio_x64-setup.exe"));
            assert!(chain.contains("os error"), "{chain}");
            assert_eq!(kind, FailureKind::Refused);
        }

        #[test]
        #[ignore]
        fn timeout() {
            // A server that accepts and never answers, against a client
            // timeout (the updater sets none today; this pins reqwest's own
            // TimedOut, the one layer recognised by its text).
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(2500));
                drop(stream);
            });
            let (chain, kind) = fetch_with(
                &format!("http://127.0.0.1:{port}/Rillio_x64-setup.exe"),
                std::time::Duration::from_millis(1000),
            );
            server.join().unwrap();
            assert_eq!(kind, FailureKind::Timeout, "{chain}");
        }

        #[test]
        #[ignore]
        fn tls_failure() {
            // An https client meeting a server that answers the ClientHello
            // with plain HTTP: rustls rejects the record.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                use std::io::{Read, Write};
                let (mut stream, _) = listener.accept().unwrap();
                let mut hello = [0u8; 512];
                let _ = stream.read(&mut hello);
                let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
                std::thread::sleep(std::time::Duration::from_millis(300));
            });
            let (chain, kind) = fetch(&format!("https://127.0.0.1:{port}/Rillio_x64-setup.exe"));
            server.join().unwrap();
            assert_eq!(kind, FailureKind::Tls, "{chain}");
        }

        #[test]
        #[ignore]
        fn connection_reset() {
            // Closing a socket with unread bytes in its receive buffer sends
            // a RST instead of a FIN: accept, let the request arrive, drop.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(300));
                drop(stream);
            });
            let (chain, kind) = fetch(&format!("http://127.0.0.1:{port}/Rillio_x64-setup.exe"));
            server.join().unwrap();
            assert!(chain.contains("os error") || chain.contains("closed"), "{chain}");
            // A FIN instead of a RST (timing) names no socket error: Other.
            if chain.contains("10054") {
                assert_eq!(kind, FailureKind::Reset);
            }
        }
    }
}
