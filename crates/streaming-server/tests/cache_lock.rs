//! One engine per cache.
//!
//! A relaunch of the desktop app while the previous process was still exiting
//! started a second full instance (2026-10-13 18:14 and 18:28, and on earlier
//! days), and its engine restored the SAME `session.json` on another port. Two
//! engines on one cache each rewrite the index and the fastresume files from
//! their own memory: the last writer drops what the other added or removed.
//!
//! `Engine::new` now takes an exclusive lock on `session/engine.lock` before it
//! reads or rewrites anything, and a second engine on the same cache does not
//! start: it fails with the reason, and starts fine once the first has stopped.

use rillio_streaming_server::Engine;

fn fresh_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rillio-cache-lock-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_engine_on_the_same_cache_does_not_start() {
    let dir = fresh_dir("second");
    let first = Engine::new(dir.clone()).await.expect("the first engine starts");
    let index_before = std::fs::read(dir.join("session").join("session.json")).ok();

    let refused = match Engine::new(dir.clone()).await {
        Ok(_) => panic!("a second engine started on a cache another engine is using"),
        Err(e) => format!("{e:#}"),
    };
    assert!(
        refused.contains("another Rillio is still using this cache"),
        "the refusal says why, in words the Cache window can show: {refused}"
    );
    assert_eq!(
        std::fs::read(dir.join("session").join("session.json")).ok(),
        index_before,
        "the refused start must not have touched the index"
    );

    // A different cache is nobody else's business.
    let other = Engine::new(fresh_dir("other")).await.expect("an engine on another cache starts");
    other.session().stop().await;

    // Once the first one has stopped, the cache can be opened again.
    first.session().stop().await;
    let mut reopened = None;
    for _ in 0..50 {
        match Engine::new(dir.clone()).await {
            Ok(engine) => {
                reopened = Some(engine);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
    let reopened = reopened.expect("the cache opens again within 5 s of the first engine stopping");
    reopened.session().stop().await;
}
