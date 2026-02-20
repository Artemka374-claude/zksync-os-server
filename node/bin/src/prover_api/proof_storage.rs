use crate::config::ProofStorageConfig;
use crate::prover_api::fri_job_manager::FailedFriProof;
use crate::prover_api::metrics::{PROOF_STORAGE_METRICS, ProofStorageMethod};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::fs;
use tokio::sync::Mutex;
use zksync_os_l1_sender::batcher_model::{FriProof, SignedBatchEnvelope};

///Persists FRI proofs to disk together with the batch if proof is successful
#[derive(Clone, Debug)]
pub struct ProofStorage {
    batches_with_proof: Arc<Mutex<BoundedFileStorage>>,
    failed: Arc<Mutex<BoundedFileStorage>>,
}
impl ProofStorage {
    // This limit is present because we scan the files on every write to get the size
    const CAPACITY_FILES: u64 = 600;
    pub fn new(config: ProofStorageConfig) -> Self {
        tracing::info!(
            path = config.path.to_str().unwrap(),
            batch_with_proof_capacity = config.batch_with_proof_capacity,
            failed_capacity = config.failed_capacity,
            "Initializing proof storage"
        );
        Self {
            batches_with_proof: Arc::new(Mutex::new(BoundedFileStorage::new(
                config.path.join("fri_batches"),
                config.batch_with_proof_capacity,
                Self::CAPACITY_FILES,
            ))),
            failed: Arc::new(Mutex::new(BoundedFileStorage::new(
                config.path.join("failed_proofs"),
                config.failed_capacity,
                Self::CAPACITY_FILES,
            ))),
        }
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
        let latency = PROOF_STORAGE_METRICS.latency[&ProofStorageMethod::SaveFailedProof].start();

        let key = format!("failed_{}.json", proof.batch_number);
        let usage = self.failed.lock().await.store(&key, proof).await?;

        PROOF_STORAGE_METRICS.disk_usage[&ProofStorageMethod::SaveFailedProof].set(usage);
        latency.observe();
        Ok(())
    }

    /// Get the failed proof for a given batch number.
    /// Returns None if no failed proof exists for this batch.
    pub async fn get_failed_proof(&self, batch_num: u64) -> anyhow::Result<Option<FailedFriProof>> {
        let latency = PROOF_STORAGE_METRICS.latency[&ProofStorageMethod::GetFailedProof].start();

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

// TODO: This is not thread-safe
// For context, this thing usually has 2 copies and only one is a writer, don't see thing changing soon
// I would either keep it or wrap in Arc<Mutex<, is there better way?
// TODO: Add metrics, I think the following ones:
// - Write latency
// - Read latency
// - Disk usage
// This would mean 2x3 = 6 new metrics OR I could unify them between instances and only ahve 3
/// Storage for data blobs that
/// automatically removes old files to keep disk usage within capacity_bytes
#[derive(Clone, Debug)]
struct BoundedFileStorage {
    base_dir: PathBuf,
    capacity_bytes: u64,
    capacity_files: u64,
}

impl BoundedFileStorage {
    fn new(base_dir: PathBuf, capacity_bytes: u64, capacity_files: u64) -> Self {
        Self {
            base_dir,
            capacity_bytes,
            capacity_files,
        }
    }

    /// Stores serialized value as a file named `key`,
    /// removes old files to enforce capacity constraints and
    /// returns disk usage
    async fn store<T: Serialize>(&self, key: &str, value: &T) -> anyhow::Result<u64> {
        fs::create_dir_all(&self.base_dir).await?;

        let file_path = self.base_dir.join(key);
        let data = serde_json::to_vec(value)?;
        let usage = self.enforce_capacity(data.len() as u64).await?;
        if (data.len() as u64) <= self.capacity_bytes {
            fs::write(file_path, data).await?;
        } else {
            tracing::warn!(
                data_len = data.len(),
                capacity = self.capacity_bytes,
                "Entry size is larger than the limit. Not saving.",
            );
        }
        Ok(usage)
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
    /// Returns disk usage
    async fn enforce_capacity(&self, new_file_size: u64) -> anyhow::Result<u64> {
        // List all files sorted by timestamp (descending)
        let mut entries = fs::read_dir(&self.base_dir).await?;
        let mut files = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let meta = entry.metadata().await?;
            if meta.is_file() {
                files.push((entry.path(), meta));
            }
        }
        files.sort_by_cached_key(|(_, meta)| {
            Reverse(meta.modified().unwrap_or(SystemTime::UNIX_EPOCH))
        });

        //Delete old files to satisfy capacity constraints
        let mut current_size = new_file_size;
        let mut current_count = 1;
        let files_to_delete = files.into_iter().skip_while(|(_, meta)| {
            current_size += meta.len();
            current_count += 1;
            current_count <= self.capacity_files && current_size <= self.capacity_bytes
        });
        for (path, _) in files_to_delete {
            fs::remove_file(path).await?;
        }
        Ok(current_size)
    }
}

//Since this data isn't used by the node itself, I added some tests
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    //Make sure files are being removed as expected
    #[tokio::test]
    async fn test_bounded_storage_capacity() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        let path = dir.path().to_owned();
        let capacity_files = 600;
        let storage = BoundedFileStorage::new(path, 1 << 20, capacity_files);

        //verify file capacity
        for i in 0..2000 {
            let str: String = i.to_string();
            storage.store(&str, &str).await?;
            assert_eq!(storage.load::<String>(str.as_str()).await?, Some(str));
            if i >= capacity_files {
                assert!(
                    storage
                        .load::<String>(&(i - capacity_files + 1).to_string())
                        .await?
                        .is_some()
                );
                assert!(
                    storage
                        .load::<String>(&(i - capacity_files).to_string())
                        .await?
                        .is_none()
                );
            }
        }

        //verify size capacity
        let big_str = "a".repeat((1 << 20) - 500);
        storage.store("key", &big_str).await?;
        //This removes most entries but not all
        assert!(storage.load::<String>(&1200.to_string()).await?.is_none());
        assert!(storage.load::<String>(&1999.to_string()).await?.is_some());
        //This should remove all the old entries
        storage.store("key2", &big_str).await?;
        assert!(storage.load::<String>("key").await?.is_none());
        //Can't store huge files -
        let very_big = "a".repeat(1 << 21);
        storage.store("key", &very_big).await?;
        assert!(storage.load::<String>("key").await?.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_bounded_storage_overwrites() -> anyhow::Result<()> {
        const LIMIT: u64 = 1 << 20;
        let dir = TempDir::new()?;
        let path = dir.path().to_owned();
        let storage = BoundedFileStorage::new(path, LIMIT);
        let big_str_a = "a".repeat((LIMIT * 2 / 3) as usize);
        storage.store("key", &big_str_a).await?;
        assert_eq!(storage.load("key").await?, Some(big_str_a));
        let big_str_b = "b".repeat((LIMIT * 2 / 3) as usize);
        storage.store("key", &big_str_b).await?;
        assert_eq!(storage.load("key").await?, Some(big_str_b));

        Ok(())
    }
}
