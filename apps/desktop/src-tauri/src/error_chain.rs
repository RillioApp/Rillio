//! Render an error WITH its cause chain.
//!
//! An error's `Display` is only its top layer. reqwest's is "error sending
//! request for url (...)" and nothing more: the part that says what actually
//! went wrong (DNS failure, TLS handshake, connection reset, timeout) lives in
//! `source()`, and `to_string()` drops it. The v0.1.44 -> v0.1.45 update
//! failure (2026-09-29) reached the screen as exactly that top line, which told
//! neither the user nor us anything. Every error the shell shows or logs for a
//! network or update failure goes through [`error_chain`].

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

    /// Real network failures through reqwest, wrapped the way the updater
    /// plugin wraps them. Ignored by default (they touch the resolver and the
    /// loopback stack, which is flaky on the dev box); run with
    /// `cargo test error_chain -- --ignored --nocapture` to print the chains.
    #[cfg(not(target_os = "android"))]
    mod probes {
        use super::super::error_chain;

        fn fetch(url: &str) -> String {
            // What tauri-plugin-updater's check() does before its first
            // request (its reqwest is built with `rustls-no-provider`).
            if rustls::crypto::CryptoProvider::get_default().is_none() {
                let _ = rustls::crypto::ring::default_provider().install_default();
            }
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                // reqwest 0.13, the updater's (see Cargo.toml dev-dependencies).
                let client = reqwest_updater::Client::builder()
                    .timeout(std::time::Duration::from_secs(20))
                    .build()
                    .unwrap();
                let result = async { client.get(url).send().await?.bytes().await }.await;
                let err = tauri_plugin_updater::Error::Reqwest(result.expect_err("the request must fail"));
                let top = err.to_string();
                let chain = error_chain(&err);
                println!("top-level Display: {top}");
                println!("error_chain:       {chain}");
                assert!(chain.len() > top.len(), "the chain must add the cause");
                chain
            })
        }

        #[test]
        #[ignore]
        fn dns_failure() {
            // `.invalid` never resolves (RFC 2606). Plain http: resolution
            // fails before any TLS would start.
            let chain = fetch("http://rillio-update-probe.invalid/Rillio_x64-setup.exe");
            assert!(chain.contains("dns error"), "{chain}");
        }

        #[test]
        #[ignore]
        fn connection_refused() {
            // Bind then drop: the port is free and nothing listens on it.
            let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            let chain = fetch(&format!("http://127.0.0.1:{port}/Rillio_x64-setup.exe"));
            assert!(chain.contains("os error"), "{chain}");
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
            let chain = fetch(&format!("http://127.0.0.1:{port}/Rillio_x64-setup.exe"));
            server.join().unwrap();
            assert!(chain.contains("os error") || chain.contains("closed"), "{chain}");
        }
    }
}
