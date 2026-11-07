//! The Client pool: one Live client per active Grant, opened lazily, evicted when idle.
//!
//! Generic over the Live client so its rules are tested without Telegram:
//! - concurrent first calls for one Grant open it once;
//! - a Live client idle for longer than the TTL is shut down and dropped, together
//!   with its decrypted Telegram session and Session key (memory per idle Account ≈ 0);
//! - if the Telegram session changed while live (DC migration, new auth keys), the
//!   new snapshot is handed to `on_evict` so it can be re-sealed in the vault.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::sync::OnceCell;
use zeroize::Zeroizing;

use super::TgError;

pub trait Live: Send + Sync + 'static {
    /// The current Telegram session bytes.
    fn snapshot(&self) -> Result<Zeroizing<Vec<u8>>, TgError>;
    /// Close connections. Called once, on eviction.
    fn shutdown(&self);
}

struct Slot<L> {
    cell: OnceCell<Arc<L>>,
    last_used: Mutex<Instant>,
    /// The snapshot it was opened with, to detect changes.
    opened_with: Mutex<Option<Zeroizing<Vec<u8>>>>,
}

/// A Live client taken out of the pool whose Telegram session changed while it
/// was live: `(key, live, new snapshot)`.
pub type Evicted<L> = (String, Arc<L>, Zeroizing<Vec<u8>>);

pub struct Pool<L> {
    slots: Mutex<HashMap<String, Arc<Slot<L>>>>,
    idle_ttl: Duration,
}

impl<L: Live> Pool<L> {
    pub fn new(idle_ttl: Duration) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            idle_ttl,
        }
    }

    /// The Live client for `key`, opening it with `open(snapshot)` if there is none.
    pub async fn get_or_open<F, Fut>(
        &self,
        key: &str,
        snapshot: &[u8],
        open: F,
    ) -> Result<Arc<L>, TgError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<L, TgError>>,
    {
        let slot = {
            let mut slots = self.slots.lock().expect("pool lock");
            slots
                .entry(key.to_string())
                .or_insert_with(|| {
                    Arc::new(Slot {
                        cell: OnceCell::new(),
                        last_used: Mutex::new(Instant::now()),
                        opened_with: Mutex::new(None),
                    })
                })
                .clone()
        };
        let live = slot
            .cell
            .get_or_try_init(|| async {
                let live = open().await?;
                *slot.opened_with.lock().expect("pool lock") =
                    Some(Zeroizing::new(snapshot.to_vec()));
                Ok::<_, TgError>(Arc::new(live))
            })
            .await;
        match live {
            Ok(live) => {
                *slot.last_used.lock().expect("pool lock") = Instant::now();
                Ok(live.clone())
            }
            Err(err) => {
                // Don't keep a dead slot around; the next call tries again.
                let mut slots = self.slots.lock().expect("pool lock");
                if slots.get(key).is_some_and(|s| Arc::ptr_eq(s, &slot)) {
                    slots.remove(key);
                }
                Err(err)
            }
        }
    }

    /// Drop a Live client now (e.g. its session was revoked). Returns it if it was open.
    pub fn remove(&self, key: &str) -> Option<Arc<L>> {
        let slot = self.slots.lock().expect("pool lock").remove(key)?;
        let live = slot.cell.get().cloned()?;
        live.shutdown();
        Some(live)
    }

    /// Shut down Live clients idle for longer than the TTL. For each one whose
    /// Telegram session changed, returns `(key, live, new snapshot)` so the caller
    /// can persist it.
    pub fn evict_idle(&self) -> Vec<Evicted<L>> {
        self.evict(|slot| {
            slot.cell.initialized()
                && slot.last_used.lock().expect("pool lock").elapsed() >= self.idle_ttl
        })
    }

    /// Shut down every Live client, however recently used, and leave the pool
    /// empty (graceful shutdown). Reports changed sessions like [`Pool::evict_idle`].
    pub fn drain(&self) -> Vec<Evicted<L>> {
        self.evict(|_| true)
    }

    fn evict(&self, pick: impl Fn(&Slot<L>) -> bool) -> Vec<Evicted<L>> {
        let picked: Vec<(String, Arc<Slot<L>>)> = {
            let mut slots = self.slots.lock().expect("pool lock");
            let keys: Vec<String> = slots
                .iter()
                .filter(|(_, slot)| pick(slot))
                .map(|(key, _)| key.clone())
                .collect();
            keys.into_iter()
                .filter_map(|key| slots.remove(&key).map(|slot| (key, slot)))
                .collect()
        };
        let mut changed = Vec::new();
        for (key, slot) in picked {
            let Some(live) = slot.cell.get().cloned() else {
                continue;
            };
            live.shutdown();
            match live.snapshot() {
                Ok(now) => {
                    let before = slot.opened_with.lock().expect("pool lock").take();
                    if before.as_deref().map(Vec::as_slice) != Some(now.as_slice()) {
                        changed.push((key, live, now));
                    }
                }
                Err(err) => tracing::warn!(error = %err, "snapshot on eviction failed"),
            }
        }
        changed
    }

    pub fn len(&self) -> usize {
        self.slots.lock().expect("pool lock").len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;

    struct FakeLive {
        session: Mutex<Vec<u8>>,
        down: AtomicBool,
    }

    impl Live for FakeLive {
        fn snapshot(&self) -> Result<Zeroizing<Vec<u8>>, TgError> {
            Ok(Zeroizing::new(self.session.lock().unwrap().clone()))
        }
        fn shutdown(&self) {
            self.down.store(true, Ordering::SeqCst);
        }
    }

    fn live(session: &[u8]) -> FakeLive {
        FakeLive {
            session: Mutex::new(session.to_vec()),
            down: AtomicBool::new(false),
        }
    }

    #[tokio::test]
    async fn concurrent_first_calls_open_once() {
        let pool = Pool::new(Duration::from_secs(60));
        let opens = AtomicUsize::new(0);
        let open = || async {
            opens.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok(live(b"s"))
        };
        let (a, b) = tokio::join!(
            pool.get_or_open("g", b"s", open),
            pool.get_or_open("g", b"s", open)
        );
        assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_open_is_retried_next_time() {
        let pool: Pool<FakeLive> = Pool::new(Duration::from_secs(60));
        let err = pool
            .get_or_open("g", b"s", || async {
                Err(TgError::Upstream("down".into()))
            })
            .await;
        assert!(err.is_err());
        assert!(pool.is_empty());
        assert!(
            pool.get_or_open("g", b"s", || async { Ok(live(b"s")) })
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn idle_clients_are_shut_down_and_only_changed_sessions_reported() {
        let pool = Pool::new(Duration::ZERO);
        let same = pool
            .get_or_open("same", b"s1", || async { Ok(live(b"s1")) })
            .await
            .unwrap();
        let moved = pool
            .get_or_open("moved", b"s2", || async { Ok(live(b"s2")) })
            .await
            .unwrap();
        *moved.session.lock().unwrap() = b"s2-after-dc-migration".to_vec();

        let changed = pool.evict_idle();
        assert!(pool.is_empty(), "idle clients must not stay in memory");
        assert!(same.down.load(Ordering::SeqCst) && moved.down.load(Ordering::SeqCst));
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, "moved");
        assert_eq!(changed[0].2.as_slice(), b"s2-after-dc-migration");
    }

    #[tokio::test]
    async fn recently_used_clients_stay() {
        let pool = Pool::new(Duration::from_secs(60));
        pool.get_or_open("g", b"s", || async { Ok(live(b"s")) })
            .await
            .unwrap();
        assert!(pool.evict_idle().is_empty());
        assert_eq!(pool.len(), 1);
    }

    #[tokio::test]
    async fn drain_shuts_down_every_client_and_reports_only_changed_sessions() {
        let pool = Pool::new(Duration::from_secs(60));
        let same = pool
            .get_or_open("same", b"s1", || async { Ok(live(b"s1")) })
            .await
            .unwrap();
        let moved = pool
            .get_or_open("moved", b"s2", || async { Ok(live(b"s2")) })
            .await
            .unwrap();
        *moved.session.lock().unwrap() = b"s2-after-dc-migration".to_vec();
        // Both were just used: an idle sweep would keep them.
        assert!(pool.evict_idle().is_empty());

        let changed = pool.drain();
        assert!(pool.is_empty(), "nothing may stay open after a drain");
        assert!(same.down.load(Ordering::SeqCst) && moved.down.load(Ordering::SeqCst));
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].0, "moved");
        assert!(Arc::ptr_eq(&changed[0].1, &moved));
        assert_eq!(changed[0].2.as_slice(), b"s2-after-dc-migration");
        assert!(pool.drain().is_empty(), "a second drain finds nothing");
    }
}
