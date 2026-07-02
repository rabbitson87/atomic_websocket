//! Stress tests guarding the "no blocking on the async runtime" invariant.
//!
//! All native-db (redb) work is synchronous and performs disk I/O — `commit()`
//! issues an `fsync`. If that runs directly on a Tokio worker thread it stalls
//! the async runtime on slow disks (low-spec machines), drifting the ping loop
//! timing and triggering false disconnections / reconnection storms.
//!
//! The library runs every DB transaction inside `spawn_blocking`. These tests
//! verify that invariant so a regression (reverting to inline `db.lock().await`
//! + synchronous commit) is caught.

#![cfg(feature = "native-db")]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use atomic_websocket::common::{get_setting_by_key, set_setting};
use atomic_websocket::external::native_db::{Builder, Models};
use atomic_websocket::types::DB;
use atomic_websocket::Settings;
use tempfile::NamedTempFile;
use tokio::sync::Mutex;

/// Builds a fresh on-disk native-db. The returned `NamedTempFile` must be kept
/// alive for the duration of the test (it owns the backing file).
fn make_native_db() -> (NamedTempFile, DB) {
    let mut models = Models::new();
    models.define::<Settings>().unwrap();
    let models: &'static Models = Box::leak(Box::new(models));

    let temp = NamedTempFile::new().unwrap();
    let db = Builder::new().create(models, temp.path()).unwrap();
    (temp, Arc::new(Mutex::new(db)))
}

/// On a single-threaded runtime, a barrage of committing DB writes must NOT
/// starve the one async worker thread. A timer task should keep ticking at
/// roughly its normal cadence throughout the write barrage.
///
/// If the synchronous `commit()` (fsync) ran inline on the worker thread, the
/// timer would be starved and observe far fewer ticks than time elapsed allows.
#[tokio::test(flavor = "current_thread")]
async fn db_writes_do_not_starve_single_thread_runtime() {
    let (_temp, db) = make_native_db();

    // Background timer on the single async thread. Sleep-based (not `interval`)
    // so there is no burst catch-up that could mask starvation.
    let ticks = Arc::new(AtomicU64::new(0));
    let ticks_clone = ticks.clone();
    let ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(5)).await;
            ticks_clone.fetch_add(1, Ordering::Relaxed);
        }
    });

    // Hammer the DB with many committing writes concurrently.
    let start = Instant::now();
    let mut handles = Vec::new();
    for i in 0..200u32 {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            set_setting(
                db,
                Settings {
                    key: format!("key-{i}"),
                    value: i.to_le_bytes().to_vec(),
                },
            )
            .await
            .expect("set_setting should succeed");
        }));
    }
    for h in handles {
        h.await.expect("write task panicked");
    }
    let elapsed = start.elapsed();

    ticker.abort();

    let observed = ticks.load(Ordering::Relaxed);
    // Theoretical max ticks for a never-blocked 5ms timer over the barrage.
    let theoretical = (elapsed.as_millis() / 5) as u64;

    // A free runtime keeps the timer near cadence (observed ≈ theoretical).
    // A starved runtime (inline fsync) observes a small fraction. Require at
    // least 25% — generous enough to never flake, strict enough to catch a
    // worker thread blocked on disk I/O.
    assert!(
        observed * 4 >= theoretical,
        "runtime starved during DB writes: {observed} ticks in {elapsed:?} \
         (theoretical {theoretical}); DB fsync is likely blocking the worker thread"
    );
}

/// Many concurrent writers and readers against one shared DB must all complete
/// (no deadlock from `blocking_lock` contention) and preserve data integrity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_concurrent_writes_are_correct_and_deadlock_free() {
    let (_temp, db) = make_native_db();

    const N: u32 = 100;

    // Concurrent writers.
    let mut writers = Vec::new();
    for i in 0..N {
        let db = db.clone();
        writers.push(tokio::spawn(async move {
            set_setting(
                db,
                Settings {
                    key: format!("k-{i}"),
                    value: i.to_le_bytes().to_vec(),
                },
            )
            .await
            .expect("write should succeed");
        }));
    }

    // Must finish well within this bound; a deadlock would hang past it.
    let writers_done = async {
        for w in writers {
            w.await.expect("writer panicked");
        }
    };
    tokio::time::timeout(Duration::from_secs(30), writers_done)
        .await
        .expect("concurrent writes deadlocked or timed out");

    // Every value must be readable and correct.
    for i in 0..N {
        let got = get_setting_by_key(db.clone(), format!("k-{i}"))
            .await
            .expect("read should succeed")
            .expect("key should exist");
        assert_eq!(got.value, i.to_le_bytes().to_vec(), "value mismatch for k-{i}");
    }
}
