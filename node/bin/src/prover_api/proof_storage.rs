use crate::config::ProofStorageConfig;
use crate::prover_api::fri_job_manager::FailedFriProof;
use crate::prover_api::metrics::{PROOF_STORAGE_METRICS, ProofStorageMethod};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::Metadata;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs;
use tokio::sync::Mutex;
use zksync_os_l1_sender::batcher_model::{FriProof, SignedBatchEnvelope};

/// Persists FRI proofs to disk together with the batch if proof is successful
#[derive(Clone, Debug)]
pub struct ProofStorage {
    batches_with_proof: Arc<Mutex<BoundedFileStorage>>,
    failed: Arc<Mutex<BoundedFileStorage>>,
}
impl ProofStorage {
    pub async fn new(config: ProofStorageConfig) -> anyhow::Result<Self> {
        tracing::info!(
            path = config.path.display().to_string(),
            batch_with_proof_capacity = config.batch_with_proof_capacity.0,
            failed_capacity = config.failed_capacity.0,
            "Initializing proof storage"
        );
        Ok(Self {
            batches_with_proof: Arc::new(Mutex::new(
                BoundedFileStorage::new(
                    config.path.join("fri_batches"),
                    config.batch_with_proof_capacity.0,
                )
                .await?,
            )),
            failed: Arc::new(Mutex::new(
                BoundedFileStorage::new(
                    config.path.join("failed_proofs"),
                    config.failed_capacity.0,
                )
                .await?,
            )),
        })
    }

    /// Persist a BatchWithProof. Overwrites any existing entry for the same batch.
    pub async fn save_batch_with_proof(&self, batch: &StoredBatch) -> anyhow::Result<()> {
        let latency =
            PROOF_STORAGE_METRICS.latency[&ProofStorageMethod::SaveBatchWithProof].start();

        let key = format!("batch_{}.json", batch.batch_number());
        let usage = self
            .batches_with_proof
            .lock()
            .await
            .store(&key, batch)
            .await?;

        PROOF_STORAGE_METRICS.disk_usage[&ProofStorageMethod::SaveBatchWithProof].set(usage);
        latency.observe();
        Ok(())
    }

    /// Loads a BatchWithProof for `batch_number`, if present
    pub async fn get_batch_with_proof(
        &self,
        batch_num: u64,
    ) -> anyhow::Result<Option<SignedBatchEnvelope<FriProof>>> {
        let latency = PROOF_STORAGE_METRICS.latency[&ProofStorageMethod::GetBatchWithProof].start();

        let key = format!("batch_{batch_num}.json");
        let result = match self
            .batches_with_proof
            .lock()
            .await
            .load::<StoredBatch>(&key)
            .await
        {
            Ok(o) => Ok(o.map(|o| o.batch_envelope())),
            Err(err) => Err(err),
        };

        latency.observe();
        result
    }

    /// Save a failed FRI proof for debugging.
    pub async fn save_failed_proof(&self, proof: &FailedFriProof) -> anyhow::Result<()> {
        let latency = PROOF_STORAGE_METRICS.latency[&ProofStorageMethod::SaveFailed].start();

        let key = format!("failed_{}.json", proof.batch_number);
        let usage = self.failed.lock().await.store(&key, proof).await?;

        PROOF_STORAGE_METRICS.disk_usage[&ProofStorageMethod::SaveFailed].set(usage);
        latency.observe();
        Ok(())
    }

    /// Get the failed proof for a given batch number.
    /// Returns None if no failed proof exists for this batch.
    pub async fn get_failed_proof(&self, batch_num: u64) -> anyhow::Result<Option<FailedFriProof>> {
        let latency = PROOF_STORAGE_METRICS.latency[&ProofStorageMethod::GetFailed].start();

        let key = format!("failed_{batch_num}.json");
        let result = self.failed.lock().await.load(&key).await;

        latency.observe();
        result
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StoredBatch {
    V1(SignedBatchEnvelope<FriProof>),
}

impl StoredBatch {
    pub fn batch_number(&self) -> u64 {
        match self {
            StoredBatch::V1(envelope) => envelope.batch_number(),
        }
    }

    pub fn batch_envelope(self) -> SignedBatchEnvelope<FriProof> {
        match self {
            StoredBatch::V1(envelope) => envelope,
        }
    }
}

/// Storage for data blobs that
/// automatically removes old files to keep disk usage within capacity_bytes
#[derive(Clone, Debug)]
struct BoundedFileStorage {
    base_dir: PathBuf,
    capacity_bytes: u64,
    current_size: u64,
    erase_queue: VecDeque<(String, Metadata)>,
    // `value` is the number of times the `key` has been moved back in `erase_queue`
    // So while this number is not zero we won't erase the file, but decrement this
    skip_cnt: HashMap<String, u64>,
}

impl BoundedFileStorage {
    async fn new(base_dir: PathBuf, capacity_bytes: u64) -> anyhow::Result<Self> {
        // List all files sorted by timestamp (descending)
        fs::create_dir_all(&base_dir).await?;
        let mut entries = fs::read_dir(&base_dir).await?;
        let mut files = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let meta = entry.metadata().await?;
            if meta.is_file() {
                files.push((entry.file_name().into_string().unwrap(), meta));
            }
        }
        files.sort_by_cached_key(|(_, meta)| meta.modified().unwrap_or(SystemTime::UNIX_EPOCH));

        let mut current_size = 0_u64;
        for (_, meta) in &files {
            current_size += meta.len();
        }

        let mut res = Self {
            base_dir,
            capacity_bytes,
            current_size,
            erase_queue: files.into_iter().collect(),
            skip_cnt: HashMap::new(),
        };

        if current_size > capacity_bytes {
            tracing::warn!(
                current_size,
                capacity_bytes,
                "On startup, more data is used than expected"
            );
            res.enforce_capacity(0).await?;
        }

        Ok(res)
    }

    /// Stores serialized value as a file named `key`,
    /// removes old files to enforce capacity constraints and
    /// returns disk usage
    async fn store<T: Serialize>(&mut self, key: &str, value: &T) -> anyhow::Result<u64> {
        fs::create_dir_all(&self.base_dir).await?;

        let data = serde_json::to_vec(value)?;
        let count = data.len() as u64;
        self.enforce_capacity(count).await?;
        self.handle_duplicate(key).await?;
        if count <= self.capacity_bytes {
            self.write_file(key, data).await?;
        } else {
            tracing::warn!(
                data_len = data.len(),
                capacity = self.capacity_bytes,
                "Entry size is larger than the limit. Not saving.",
            );
        }
        Ok(self.current_size)
    }

    async fn load<T: DeserializeOwned>(&self, key: &str) -> anyhow::Result<Option<T>> {
        let path = self.base_dir.join(key);
        if !path.exists() {
            return Ok(None);
        }

        let data = fs::read(path).await?;
        let decoded = serde_json::from_slice(&data)?;
        Ok(Some(decoded))
    }

    /// Delete old files to make space for the new file
    async fn enforce_capacity(&mut self, new_file_size: u64) -> anyhow::Result<()> {
        // Delete old files to satisfy capacity constraints
        while self.current_size + new_file_size > self.capacity_bytes
            && !self.erase_queue.is_empty()
        {
            let (key, meta) = self.erase_queue.pop_front().unwrap();
            if let Some(duplicates) = self.skip_cnt.get_mut(&key)
                && *duplicates > 0
            {
                *duplicates -= 1;
                continue;
            }

            fs::remove_file(self.base_dir.join(key)).await?;
            self.current_size -= meta.len();
        }
        Ok(())
    }

    /// If present, file at this path is renamed and moved to the end of the queue.
    async fn handle_duplicate(&mut self, key: &str) -> anyhow::Result<()> {
        let path = self.base_dir.join(key);
        if path.is_file() {
            tracing::info!("Storing old version of {}", key);

            // Delete the file
            let old_data = fs::read(&path).await?;
            fs::remove_file(&path).await?;
            self.current_size -= old_data.len() as u64;
            *self.skip_cnt.entry(key.to_string()).or_insert(0) += 1;

            // Save it again under a different name
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            self.write_file(&format!("{key}.overwritten_{now}"), old_data)
                .await?;
        }
        Ok(())
    }

    /// Write file to disk and add it to erase_queue
    async fn write_file(&mut self, key: &str, data: Vec<u8>) -> anyhow::Result<()> {
        let path = self.base_dir.join(key);
        let len = data.len() as u64;
        fs::write(&path, data).await?;
        self.current_size += len;
        let meta = fs::metadata(&path).await?;
        self.erase_queue.push_back((key.to_string(), meta));
        Ok(())
    }
}

// Since this data isn't used by the node itself, I added some tests
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Make sure files are being removed as expected
    #[tokio::test]
    async fn test_bounded_storage_capacity() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        let path = dir.path().to_owned();
        const LIMIT: u64 = 20000;
        let mut storage = BoundedFileStorage::new(path, LIMIT).await?;

        // Many small files
        let num_iter = 2000;
        for i in 0..num_iter {
            let key: String = i.to_string();
            let val = "a".repeat((LIMIT / num_iter) as usize);
            storage.store(&key, &val).await?;
            assert_eq!(storage.load::<String>(key.as_str()).await?, Some(val));
            if i >= num_iter {
                assert!(
                    storage
                        .load::<String>(&(i - num_iter + 1).to_string())
                        .await?
                        .is_some()
                );
                assert!(
                    storage
                        .load::<String>(&(i - num_iter).to_string())
                        .await?
                        .is_none()
                );
            }
        }

        // Large files
        let big_str = "a".repeat((LIMIT * 2 / 3) as usize);
        storage.store("key", &big_str).await?;
        // This removes most entries but not all
        assert!(
            storage
                .load::<String>(&(num_iter / 2).to_string())
                .await?
                .is_none()
        );
        assert!(
            storage
                .load::<String>(&(num_iter - 1).to_string())
                .await?
                .is_some()
        );
        // This should remove all the old entries
        storage.store("key2", &big_str).await?;
        assert!(storage.load::<String>("key").await?.is_none());
        // Can't store huge files -
        let very_big = "a".repeat((2 * LIMIT) as usize);
        storage.store("key", &very_big).await?;
        assert!(storage.load::<String>("key").await?.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_bounded_storage_overwrites() -> anyhow::Result<()> {
        const LIMIT: u64 = 1 << 20;
        let dir = TempDir::new()?;
        let path = dir.path().to_owned();
        let mut storage = BoundedFileStorage::new(path, LIMIT).await?;
        // overrides in case of large strings
        let big_str_a = "a".repeat((LIMIT * 2 / 3) as usize);
        storage.store("key", &big_str_a).await?;
        assert_eq!(storage.load("key").await?, Some(big_str_a));
        let big_str_b = "b".repeat((LIMIT * 2 / 3) as usize);
        storage.store("key", &big_str_b).await?;
        assert_eq!(storage.load("key").await?, Some(big_str_b));
        Ok(())
    }

    #[tokio::test]
    async fn test_bounded_storage_overwrite_cleanup() -> anyhow::Result<()> {
        const LIMIT: u64 = 506;
        let dir = TempDir::new()?;
        let path = dir.path().to_owned();
        let mut storage = BoundedFileStorage::new(path, LIMIT).await?;

        let str1 = "a".repeat(100);
        let str2 = "ab".repeat(100);
        storage.store("0", &str2).await?;
        storage.store("1", &str2).await?;
        storage.store("0", &str1).await?;
        // TODO: handle acse when overwrite is the same value
        storage.store("0", &str2).await?;
        assert_eq!(storage.load::<String>("1").await?, None);
        storage.store("1", &str2).await?;
        // Duplicate was removed here
        assert!(storage.load::<String>("0").await?.is_some());
        assert!(storage.load::<String>("1").await?.is_some());

        Ok(())
    }
}
