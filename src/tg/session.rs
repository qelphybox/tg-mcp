//! A grammers `Session` that lives in memory and exports a minimal snapshot.
//!
//! The snapshot is what the Session vault seals as the Grant's "Telegram session":
//! the home DC and the per-DC auth keys. Peer caches and update state stay in
//! memory only (no chat metadata at rest, and we never process updates).

use std::{
    fmt,
    sync::{Mutex, MutexGuard},
};

use grammers_session::{
    BoxFuture, Session, SessionData,
    types::{DcOption, PeerId, PeerInfo, UpdateState, UpdatesState},
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::TgError;

/// Bump when the snapshot format changes incompatibly.
const SNAPSHOT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Snapshot {
    v: u32,
    home_dc: i32,
    dc_options: Vec<DcOption>,
}

pub struct SessionStore(Mutex<SessionData>);

#[derive(Debug)]
pub struct Poisoned;

impl fmt::Display for Poisoned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("session lock is poisoned")
    }
}

impl std::error::Error for Poisoned {}

impl SessionStore {
    /// A fresh session: Telegram's known DCs, no auth keys, not logged in.
    pub fn new() -> Self {
        Self(Mutex::new(SessionData::default()))
    }

    /// Restore from a snapshot made by [`SessionStore::snapshot`].
    pub fn from_snapshot(bytes: &[u8]) -> Result<Self, TgError> {
        let snapshot: Snapshot = serde_json::from_slice(bytes)
            .map_err(|e| TgError::BadSession(format!("unreadable snapshot: {e}")))?;
        if snapshot.v != SNAPSHOT_VERSION {
            return Err(TgError::BadSession(format!(
                "snapshot version {} is not supported",
                snapshot.v
            )));
        }
        let mut data = SessionData {
            home_dc: snapshot.home_dc,
            ..SessionData::default()
        };
        for dc in snapshot.dc_options {
            data.dc_options.insert(dc.id, dc);
        }
        Ok(Self(Mutex::new(data)))
    }

    /// The bytes to seal in the vault. Contains auth keys: treat as a secret.
    pub fn snapshot(&self) -> Result<Zeroizing<Vec<u8>>, TgError> {
        let data = self
            .data()
            .map_err(|e| TgError::BadSession(e.to_string()))?;
        let mut dc_options: Vec<DcOption> = data.dc_options.values().cloned().collect();
        dc_options.sort_by_key(|dc| dc.id);
        let snapshot = Snapshot {
            v: SNAPSHOT_VERSION,
            home_dc: data.home_dc,
            dc_options,
        };
        Ok(Zeroizing::new(
            serde_json::to_vec(&snapshot).expect("snapshot serializes"),
        ))
    }

    fn data(&self) -> Result<MutexGuard<'_, SessionData>, Poisoned> {
        self.0.lock().map_err(|_| Poisoned)
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

// Mirrors grammers' MemorySession, which has no way to read its data back out.
impl Session for SessionStore {
    type Error = Poisoned;

    fn home_dc_id(&self) -> Result<i32, Poisoned> {
        Ok(self.data()?.home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), Poisoned>> {
        Box::pin(async move {
            self.data()?.home_dc = dc_id;
            Ok(())
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, Poisoned> {
        Ok(self.data()?.dc_options.get(&dc_id).cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), Poisoned>> {
        let dc_option = dc_option.clone();
        Box::pin(async move {
            self.data()?.dc_options.insert(dc_option.id, dc_option);
            Ok(())
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, Poisoned>> {
        Box::pin(async move { Ok(self.data()?.peer_infos.get(&peer).cloned()) })
    }

    fn cache_peer(&self, peer: PeerInfo) -> BoxFuture<'_, Result<(), Poisoned>> {
        Box::pin(async move {
            self.data()?
                .peer_infos
                .entry(peer.id())
                .or_insert_with(|| peer.clone())
                .extend_info(&peer);
            Ok(())
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, Poisoned>> {
        Box::pin(async move { Ok(self.data()?.updates_state.clone()) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), Poisoned>> {
        Box::pin(async move {
            let mut data = self.data()?;
            match update {
                UpdateState::All(state) => data.updates_state = state,
                UpdateState::Primary { pts, date, seq } => {
                    data.updates_state.pts = pts;
                    data.updates_state.date = date;
                    data.updates_state.seq = seq;
                }
                UpdateState::Secondary { qts } => data.updates_state.qts = qts,
                UpdateState::Channel { id, pts } => {
                    data.updates_state.channels.retain(|c| c.id != id);
                    data.updates_state
                        .channels
                        .push(grammers_session::types::ChannelState { id, pts });
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_round_trips_home_dc_and_auth_keys_only() {
        let store = SessionStore::new();
        store.set_home_dc_id(4).await.unwrap();
        let mut dc = store.dc_option(4).unwrap().expect("known DC 4");
        dc.auth_key = Some([7u8; 256]);
        store.set_dc_option(&dc).await.unwrap();

        let restored = SessionStore::from_snapshot(&store.snapshot().unwrap()).unwrap();
        assert_eq!(restored.home_dc_id().unwrap(), 4);
        assert_eq!(
            restored.dc_option(4).unwrap().unwrap().auth_key,
            Some([7u8; 256])
        );
        // DCs without keys survive too, so the client can still reach them.
        assert!(restored.dc_option(2).unwrap().is_some());
    }

    #[test]
    fn garbage_and_future_versions_are_bad_sessions() {
        for bad in [&b"not json"[..], br#"{"v":99,"home_dc":2,"dc_options":[]}"#] {
            assert!(matches!(
                SessionStore::from_snapshot(bad),
                Err(TgError::BadSession(_))
            ));
        }
    }
}
