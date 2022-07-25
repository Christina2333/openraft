#[cfg(test)]
mod test;

use std::cmp::max;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt::Debug;
use std::io::Cursor;
use std::ops::RangeBounds;
use std::sync::Arc;
use std::sync::Mutex;

use openraft::async_trait::async_trait;
use openraft::raft::Entry;
use openraft::raft::EntryPayload;
use openraft::raft::Membership;
use openraft::storage::HardState;
use openraft::storage::InitialState;
use openraft::storage::Snapshot;
use openraft::AnyError;
use openraft::AppData;
use openraft::AppDataResponse;
use openraft::EffectiveMembership;
use openraft::ErrorSubject;
use openraft::ErrorVerb;
use openraft::LogId;
use openraft::NodeId;
use openraft::RaftStorage;
use openraft::RaftStorageDebug;
use openraft::SnapshotMeta;
use openraft::StateMachineChanges;
use openraft::StorageError;
use openraft::StorageIOError;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::RwLock;

/// The application data request type which the `MemStore` works with.
///
/// Conceptually, for demo purposes, this represents an update to a client's status info,
/// returning the previously recorded status.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClientRequest {
    /// The ID of the client which has sent the request.
    pub client: String,
    /// The serial number of this request.
    pub serial: u64,
    /// A string describing the status of the client. For a real application, this should probably
    /// be an enum representing all of the various types of requests / operations which a client
    /// can perform.
    pub status: String,
}

impl AppData for ClientRequest {}

/// The application data response type which the `MemStore` works with.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClientResponse(Option<String>);

impl AppDataResponse for ClientResponse {}

/// The application snapshot type which the `MemStore` works with.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MemStoreSnapshot {
    pub meta: SnapshotMeta,

    /// The data of the state machine at the time of this snapshot.
    pub data: Vec<u8>,
}

/// The state machine of the `MemStore`.
#[derive(Serialize, Deserialize, Debug, Default, Clone)]
pub struct MemStoreStateMachine {
    pub last_applied_log: LogId,

    pub last_membership: Option<EffectiveMembership>,

    /// A mapping of client IDs to their state info.
    pub client_serial_responses: HashMap<String, (u64, Option<String>)>,
    /// The current status of a client by ID.
    pub client_status: HashMap<String, String>,
}

/// An in-memory storage system implementing the `RaftStorage` trait.
pub struct MemStore {
    /// The ID of the Raft node for which this memory storage instances is configured.
    id: NodeId,
    /// The Raft log.
    log: RwLock<BTreeMap<u64, Entry<ClientRequest>>>,
    /// The Raft state machine.
    sm: RwLock<MemStoreStateMachine>,
    /// The current hard state.
    hs: RwLock<Option<HardState>>,

    snapshot_idx: Arc<Mutex<u64>>,
    /// The current snapshot.
    current_snapshot: RwLock<Option<MemStoreSnapshot>>,
}

impl MemStore {
    /// Create a new `MemStore` instance.
    /// TODO(xp): creating a store should not require an id.
    pub async fn new(id: NodeId) -> Self {
        let log = RwLock::new(BTreeMap::new());
        let sm = RwLock::new(MemStoreStateMachine::default());
        let hs = RwLock::new(None);
        let current_snapshot = RwLock::new(None);

        {
            let mut l = log.write().await;
            l.insert(0, Entry {
                log_id: LogId::default(),
                payload: EntryPayload::Blank,
            });
        }

        Self {
            id,
            log,
            sm,
            hs,
            snapshot_idx: Arc::new(Mutex::new(0)),
            current_snapshot,
        }
    }

    /// Create a new `MemStore` instance with some existing state (for testing).
    #[cfg(test)]
    pub fn new_with_state(
        id: NodeId,
        log: BTreeMap<u64, Entry<ClientRequest>>,
        sm: MemStoreStateMachine,
        hs: Option<HardState>,
        current_snapshot: Option<MemStoreSnapshot>,
    ) -> Self {
        let log = RwLock::new(log);
        let sm = RwLock::new(sm);
        let hs = RwLock::new(hs);
        let current_snapshot = RwLock::new(current_snapshot);
        Self {
            id,
            log,
            sm,
            hs,
            snapshot_idx: Arc::new(Mutex::new(0)),
            current_snapshot,
        }
    }
}

#[async_trait]
impl RaftStorageDebug<MemStoreStateMachine> for MemStore {
    /// Get a handle to the state machine for testing purposes.
    async fn get_state_machine(&self) -> MemStoreStateMachine {
        self.sm.write().await.clone()
    }
}

impl MemStore {
    fn find_first_membership_log<'a, T, D>(mut it: T) -> Option<EffectiveMembership>
    where
        T: 'a + Iterator<Item = &'a Entry<D>>,
        D: AppData,
    {
        it.find_map(|entry| match &entry.payload {
            EntryPayload::Membership(cfg) => Some(EffectiveMembership {
                log_id: entry.log_id,
                membership: cfg.clone(),
            }),
            _ => None,
        })
    }

    /// Go backwards through the log to find the most recent membership config <= `upto_index`.
    #[tracing::instrument(level = "trace", skip(self))]
    pub async fn get_membership_from_log(&self, upto_index: Option<u64>) -> Result<EffectiveMembership, StorageError> {
        let membership_in_log = {
            let log = self.log.read().await;

            let reversed_logs = log.values().rev();
            match upto_index {
                Some(upto) => {
                    let skipped = reversed_logs.skip_while(|entry| entry.log_id.index > upto);
                    Self::find_first_membership_log(skipped)
                }
                None => Self::find_first_membership_log(reversed_logs),
            }
        };

        // Find membership stored in state machine.

        let (_, membership_in_sm) = self.last_applied_state().await?;

        let membership =
            if membership_in_log.as_ref().map(|x| x.log_id.index) > membership_in_sm.as_ref().map(|x| x.log_id.index) {
                membership_in_log
            } else {
                membership_in_sm
            };

        // Create a default one if both are None.

        Ok(match membership {
            Some(x) => x,
            None => EffectiveMembership {
                log_id: LogId { term: 0, index: 0 },
                membership: Membership::new_initial(self.id),
            },
        })
    }
}

#[async_trait]
impl RaftStorage<ClientRequest, ClientResponse> for MemStore {
    type SnapshotData = Cursor<Vec<u8>>;

    #[tracing::instrument(level = "trace", skip(self))]
    async fn get_membership_config(&self) -> Result<EffectiveMembership, StorageError> {
        self.get_membership_from_log(None).await
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn get_initial_state(&self) -> Result<InitialState, StorageError> {
        let membership = self.get_membership_config().await?;
        let mut hs = self.hs.write().await;
        match &mut *hs {
            Some(inner) => {
                // Search for two place and use the max one,
                // because when a state machine is installed there could be logs
                // included in the state machine that are not cleaned:
                // - the last log id
                // - the last_applied log id in state machine.

                let last_in_log = self.last_id_in_log().await?;
                let (last_applied, _) = self.last_applied_state().await?;

                let last_log_id = max(last_in_log, last_applied);

                Ok(InitialState {
                    last_log_id,
                    last_applied,
                    hard_state: inner.clone(),
                    last_membership: membership,
                })
            }
            None => {
                let new = InitialState::new_initial(self.id);
                *hs = Some(new.hard_state.clone());
                Ok(new)
            }
        }
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn save_hard_state(&self, hs: &HardState) -> Result<(), StorageError> {
        tracing::debug!(?hs, "save_hard_state");
        let mut h = self.hs.write().await;

        *h = Some(hs.clone());
        Ok(())
    }

    async fn read_hard_state(&self) -> Result<Option<HardState>, StorageError> {
        Ok(self.hs.read().await.clone())
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn get_log_entries<RNG: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &self,
        range: RNG,
    ) -> Result<Vec<Entry<ClientRequest>>, StorageError> {
        let res = {
            let log = self.log.read().await;
            log.range(range.clone()).map(|(_, val)| val.clone()).collect::<Vec<_>>()
        };

        Ok(res)
    }

    async fn try_get_log_entries<RNG: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &self,
        range: RNG,
    ) -> Result<Vec<Entry<ClientRequest>>, StorageError> {
        let res = {
            let log = self.log.read().await;
            log.range(range.clone()).map(|(_, val)| val.clone()).collect::<Vec<_>>()
        };

        Ok(res)
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn try_get_log_entry(&self, log_index: u64) -> Result<Option<Entry<ClientRequest>>, StorageError> {
        let log = self.log.read().await;
        Ok(log.get(&log_index).cloned())
    }

    async fn first_id_in_log(&self) -> Result<Option<LogId>, StorageError> {
        let log = self.log.read().await;
        let first = log.iter().next().map(|(_, ent)| ent.log_id);
        Ok(first)
    }

    async fn first_known_log_id(&self) -> Result<LogId, StorageError> {
        let first = self.first_id_in_log().await?;
        let (last_applied, _) = self.last_applied_state().await?;

        if let Some(x) = first {
            return Ok(std::cmp::min(x, last_applied));
        }

        Ok(last_applied)
    }

    async fn last_id_in_log(&self) -> Result<LogId, StorageError> {
        let log = self.log.read().await;
        let last = log.iter().last().map(|(_, ent)| ent.log_id).unwrap_or_default();
        Ok(last)
    }

    async fn last_applied_state(&self) -> Result<(LogId, Option<EffectiveMembership>), StorageError> {
        let sm = self.sm.read().await;
        Ok((sm.last_applied_log, sm.last_membership.clone()))
    }

    #[tracing::instrument(level = "trace", skip(self, range), fields(range=?range))]
    async fn delete_logs_from<R: RangeBounds<u64> + Clone + Debug + Send + Sync>(
        &self,
        range: R,
    ) -> Result<(), StorageError> {
        {
            tracing::debug!("delete_logs_from: {:?}", range);

            let mut log = self.log.write().await;

            let keys = log.range(range).map(|(k, _v)| *k).collect::<Vec<_>>();
            for key in keys {
                log.remove(&key);
            }
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self, entries))]
    async fn append_to_log(&self, entries: &[&Entry<ClientRequest>]) -> Result<(), StorageError> {
        let mut log = self.log.write().await;
        for entry in entries {
            log.insert(entry.log_id.index, (*entry).clone());
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip(self, entries))]
    async fn apply_to_state_machine(
        &self,
        entries: &[&Entry<ClientRequest>],
    ) -> Result<Vec<ClientResponse>, StorageError> {
        let mut sm = self.sm.write().await;
        let mut res = Vec::with_capacity(entries.len());

        for entry in entries {
            tracing::debug!("id:{} replicate to sm index:{}", self.id, entry.log_id.index);

            sm.last_applied_log = entry.log_id;

            match entry.payload {
                EntryPayload::Blank => res.push(ClientResponse(None)),
                EntryPayload::Normal(ref data) => {
                    if let Some((serial, r)) = sm.client_serial_responses.get(&data.client) {
                        if serial == &data.serial {
                            res.push(ClientResponse(r.clone()));
                            continue;
                        }
                    }
                    let previous = sm.client_status.insert(data.client.clone(), data.status.clone());
                    sm.client_serial_responses.insert(data.client.clone(), (data.serial, previous.clone()));
                    res.push(ClientResponse(previous));
                }
                EntryPayload::Membership(ref mem) => {
                    sm.last_membership = Some(EffectiveMembership {
                        log_id: entry.log_id,
                        membership: mem.clone(),
                    });
                    res.push(ClientResponse(None))
                }
            };
        }
        Ok(res)
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn do_log_compaction(&self) -> Result<Snapshot<Self::SnapshotData>, StorageError> {
        let (data, last_applied_log);

        {
            // Serialize the data of the state machine.
            let sm = self.sm.read().await;
            data = serde_json::to_vec(&*sm)
                .map_err(|e| StorageIOError::new(ErrorSubject::StateMachine, ErrorVerb::Read, AnyError::new(&e)))?;

            last_applied_log = sm.last_applied_log;
        }

        let snapshot_size = data.len();

        let snapshot_idx = {
            let mut l = self.snapshot_idx.lock().unwrap();
            *l += 1;
            *l
        };

        let meta;
        {
            let mut current_snapshot = self.current_snapshot.write().await;

            let snapshot_id = format!("{}-{}-{}", last_applied_log.term, last_applied_log.index, snapshot_idx);

            meta = SnapshotMeta {
                last_log_id: last_applied_log,
                snapshot_id,
            };

            let snapshot = MemStoreSnapshot {
                meta: meta.clone(),
                data: data.clone(),
            };

            *current_snapshot = Some(snapshot);
        } // Release log & snapshot write locks.

        tracing::info!({ snapshot_size = snapshot_size }, "log compaction complete");
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(data)),
        })
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn begin_receiving_snapshot(&self) -> Result<Box<Self::SnapshotData>, StorageError> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    #[tracing::instrument(level = "trace", skip(self, snapshot))]
    async fn finalize_snapshot_installation(
        &self,
        meta: &SnapshotMeta,
        snapshot: Box<Self::SnapshotData>,
    ) -> Result<StateMachineChanges, StorageError> {
        tracing::info!(
            { snapshot_size = snapshot.get_ref().len() },
            "decoding snapshot for installation"
        );

        let new_snapshot = MemStoreSnapshot {
            meta: meta.clone(),
            data: snapshot.into_inner(),
        };

        {
            let t = &new_snapshot.data;
            let y = std::str::from_utf8(t).unwrap();
            tracing::debug!("SNAP META:{:?}", meta);
            tracing::debug!("JSON SNAP DATA:{}", y);
        }

        // Update the state machine.
        {
            let new_sm: MemStoreStateMachine = serde_json::from_slice(&new_snapshot.data).map_err(|e| {
                StorageIOError::new(
                    ErrorSubject::Snapshot(new_snapshot.meta.clone()),
                    ErrorVerb::Read,
                    AnyError::new(&e),
                )
            })?;
            let mut sm = self.sm.write().await;
            *sm = new_sm;
        }

        // Update current snapshot.
        let mut current_snapshot = self.current_snapshot.write().await;
        *current_snapshot = Some(new_snapshot);
        Ok(StateMachineChanges {
            last_applied: Some(meta.last_log_id),
            is_snapshot: true,
        })
    }

    #[tracing::instrument(level = "trace", skip(self))]
    async fn get_current_snapshot(&self) -> Result<Option<Snapshot<Self::SnapshotData>>, StorageError> {
        match &*self.current_snapshot.read().await {
            Some(snapshot) => {
                // TODO(xp): try not to clone the entire data.
                //           If snapshot.data is Arc<T> that impl AsyncRead etc then the sharing can be done.
                let data = snapshot.data.clone();
                Ok(Some(Snapshot {
                    meta: snapshot.meta.clone(),
                    snapshot: Box::new(Cursor::new(data)),
                }))
            }
            None => Ok(None),
        }
    }
}
