// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::BTreeMap;
use std::sync::Arc;

use itertools::Itertools;
use risingwave_common::config::StorageConfig;
use risingwave_pb::hummock::SstableInfo;
use risingwave_rpc_client::HummockMetaClient;

use crate::error::StorageResult;
use crate::hummock::compactor::{Compactor, CompactorContext};
use crate::hummock::conflict_detector::ConflictDetector;
use crate::hummock::local_version_manager::LocalVersionManager;
use crate::hummock::shared_buffer::shared_buffer_batch::SharedBufferBatch;
use crate::hummock::{HummockError, HummockResult, SstableStoreRef};
use crate::monitor::StateStoreMetrics;
use crate::store::StorageTableId;

#[derive(Debug)]
pub struct SyncItem {
    /// Epoch to sync. `None` means syncing all epochs.
    pub(super) epoch: Option<u64>,
    /// Notifier to notify on sync finishes
    pub(super) notifier: Option<tokio::sync::oneshot::Sender<HummockResult<()>>>,
    /// Table ids to sync. `None` means syncing all tables.
    pub(super) table_ids: Option<Vec<StorageTableId>>,
}

#[derive(Debug)]
pub enum SharedBufferUploaderItem {
    Batch(SharedBufferBatch, StorageTableId),
    Sync(SyncItem),
    Reset(u64),
}

pub struct SharedBufferUploader {
    /// Batches to upload grouped by epoch and table_id
    batches_to_upload: BTreeMap<u64, BTreeMap<StorageTableId, Vec<SharedBufferBatch>>>,
    local_version_manager: Arc<LocalVersionManager>,
    options: Arc<StorageConfig>,

    /// Statistics.
    // TODO: separate `HummockStats` from `StateStoreMetrics`.
    stats: Arc<StateStoreMetrics>,
    hummock_meta_client: Arc<dyn HummockMetaClient>,
    sstable_store: SstableStoreRef,

    /// For conflict key detection. Enabled by setting `write_conflict_detection_enabled` to true
    /// in `StorageConfig`
    write_conflict_detector: Option<Arc<ConflictDetector>>,

    rx: tokio::sync::mpsc::UnboundedReceiver<SharedBufferUploaderItem>,
}

impl SharedBufferUploader {
    pub fn new(
        options: Arc<StorageConfig>,
        local_version_manager: Arc<LocalVersionManager>,
        sstable_store: SstableStoreRef,
        stats: Arc<StateStoreMetrics>,
        hummock_meta_client: Arc<dyn HummockMetaClient>,
        rx: tokio::sync::mpsc::UnboundedReceiver<SharedBufferUploaderItem>,
    ) -> Self {
        Self {
            batches_to_upload: BTreeMap::new(),
            options: options.clone(),
            local_version_manager,

            stats,
            hummock_meta_client,
            sstable_store,
            write_conflict_detector: if options.write_conflict_detection_enabled {
                Some(Arc::new(ConflictDetector::new()))
            } else {
                None
            },
            rx,
        }
    }

    /// Uploads buffer batches to S3.
    async fn sync(
        &mut self,
        epoch: u64,
        table_ids: Option<Vec<StorageTableId>>,
    ) -> HummockResult<()> {
        if let Some(detector) = &self.write_conflict_detector {
            detector.archive_epoch(epoch, &table_ids);
        }

        // TODO: may want to recover the removed batches in case of compaction failure
        let buffers = match self.batches_to_upload.get_mut(&epoch) {
            Some(table_batches) => {
                if let Some(table_ids) = table_ids {
                    let mut ret = vec![];
                    for table_id in table_ids {
                        if let Some(batches) = table_batches.remove(&table_id) {
                            ret.extend(batches);
                        } else {
                            tracing::warn!("no table for table_id `{}` exists to sync", table_id)
                        }
                    }
                    if table_batches.is_empty() {
                        self.batches_to_upload.remove(&epoch);
                    }
                    ret
                } else {
                    self.batches_to_upload
                        .remove(&epoch)
                        // existence of `epoch` has previously been checked, so it's safe to use
                        // unwrap
                        .unwrap()
                        .into_values()
                        .flat_map(Vec::into_iter)
                        .collect_vec()
                }
            }
            None => return Ok(()),
        };

        // Compact buffers into SSTs
        let mem_compactor_ctx = CompactorContext {
            options: self.options.clone(),
            local_version_manager: self.local_version_manager.clone(),
            hummock_meta_client: self.hummock_meta_client.clone(),
            sstable_store: self.sstable_store.clone(),
            stats: self.stats.clone(),
            is_share_buffer_compact: true,
        };

        let tables = Compactor::compact_shared_buffer(
            Arc::new(mem_compactor_ctx),
            buffers,
            self.stats.clone(),
        )
        .await?;

        // Add all tables at once.
        let version = self
            .hummock_meta_client
            .add_tables(
                epoch,
                tables
                    .iter()
                    .map(|sst| SstableInfo {
                        id: sst.id,
                        key_range: Some(risingwave_pb::hummock::KeyRange {
                            left: sst.meta.smallest_key.clone(),
                            right: sst.meta.largest_key.clone(),
                            inf: false,
                        }),
                    })
                    .collect(),
            )
            .await
            .map_err(HummockError::meta_error)?;

        // Ensure the added data is available locally
        self.local_version_manager.try_set_version(version);

        Ok(())
    }

    async fn handle(&mut self, item: SharedBufferUploaderItem) -> StorageResult<()> {
        match item {
            SharedBufferUploaderItem::Batch(m, table_id) => {
                if let Some(detector) = &self.write_conflict_detector {
                    detector.check_conflict_and_track_write_batch(&m.inner, m.epoch, table_id);
                }

                self.batches_to_upload
                    .entry(m.epoch())
                    .or_insert(BTreeMap::new())
                    .entry(table_id)
                    .or_insert(Vec::new())
                    .push(m);
                Ok(())
            }
            SharedBufferUploaderItem::Sync(sync_item) => {
                let res = match sync_item.epoch {
                    Some(e) => {
                        // Sync a specific epoch
                        self.sync(e, sync_item.table_ids).await
                    }
                    None => {
                        // Sync all epochs
                        let epochs = self.batches_to_upload.keys().copied().collect_vec();
                        let mut res = Ok(());
                        for e in epochs {
                            res = self.sync(e, sync_item.table_ids.clone()).await;
                            if res.is_err() {
                                break;
                            }
                        }
                        res
                    }
                };

                if let Some(tx) = sync_item.notifier {
                    tx.send(res).map_err(|_| {
                        HummockError::shared_buffer_error(
                            "Failed to notify shared buffer sync because of send drop",
                        )
                    })?;
                }
                Ok(())
            }
            SharedBufferUploaderItem::Reset(epoch) => {
                self.batches_to_upload.remove(&epoch);
                Ok(())
            }
        }
    }

    pub async fn run(mut self) -> StorageResult<()> {
        while let Some(m) = self.rx.recv().await {
            if let Err(e) = self.handle(m).await {
                return Err(e);
            }
        }
        Ok(())
    }
}
