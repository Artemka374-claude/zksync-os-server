use alloy::consensus::SignableTransaction;
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::signers::Signer;
use alloy::signers::k256::ecdsa::{Signature as K256Signature, SigningKey, VerifyingKey};
use alloy::signers::local::PrivateKeySigner;
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use k256::pkcs8::DecodePublicKey;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

type Signature = alloy::primitives::Signature;

const KMS_SCOPE: &[&str] = &["https://www.googleapis.com/auth/cloudkms"];

/// Maximum number of retry attempts for KMS signing requests.
const KMS_SIGN_MAX_RETRIES: u32 = 3;
/// Base delay between KMS signing retries (doubles on each attempt).
const KMS_SIGN_RETRY_BASE_DELAY: Duration = Duration::from_millis(200);

/// Configuration for how an operator signing key is provided.
#[derive(Clone)]
pub enum OperatorSignerConfig {
    /// Use a local private key for signing.
    Local(SigningKey),
    /// Use a Google Cloud KMS key for signing.
    GcpKms {
        /// Full resource name of the KMS key version, e.g.
        /// `projects/{project}/locations/{location}/keyRings/{ring}/cryptoKeys/{key}/cryptoKeyVersions/{version}`
        resource_name: String,
    },
}

impl fmt::Debug for OperatorSignerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(_) => f.debug_tuple("Local").field(&"[REDACTED]").finish(),
            Self::GcpKms { resource_name } => f
                .debug_struct("GcpKms")
                .field("resource_name", resource_name)
                .finish(),
        }
    }
}

impl PartialEq for OperatorSignerConfig {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local(a), Self::Local(b)) => a == b,
            (Self::GcpKms { resource_name: a }, Self::GcpKms { resource_name: b }) => a == b,
            // Cross-variant comparison: a Local key and a KMS key could theoretically
            // resolve to the same Ethereum address, but detecting that requires an async
            // KMS call. We return false here; the system will catch same-address conflicts
            // at runtime when registering signers with the wallet.
            _ => false,
        }
    }
}

impl OperatorSignerConfig {
    /// Resolves signer configuration from optional local key and KMS resource fields.
    /// KMS resource takes priority if both are set.
    pub fn resolve(sk: &Option<SigningKey>, kms_resource: &Option<String>) -> Option<Self> {
        if let Some(resource) = kms_resource {
            if sk.is_some() {
                tracing::warn!("Both local signing key and KMS resource are configured; using KMS");
            }
            Some(Self::GcpKms {
                resource_name: resource.clone(),
            })
        } else {
            sk.as_ref().map(|sk| Self::Local(sk.clone()))
        }
    }

    /// Creates the appropriate signer, registers it with the wallet, and returns the Ethereum address.
    pub async fn register_with_wallet(
        &self,
        wallet: &mut EthereumWallet,
    ) -> anyhow::Result<Address> {
        match self {
            Self::Local(sk) => {
                let signer = PrivateKeySigner::from_signing_key(sk.clone());
                let address = signer.address();
                wallet.register_signer(signer);
                Ok(address)
            }
            Self::GcpKms { resource_name } => {
                let signer = GcpKmsSigner::new(resource_name.clone()).await?;
                let address = alloy::signers::Signer::address(&signer);
                wallet.register_signer(signer);
                Ok(address)
            }
        }
    }
}

/// A signer that uses Google Cloud KMS for ECDSA secp256k1 signing.
///
/// The KMS key must be created with purpose `ASYMMETRIC_SIGN` and algorithm
/// `EC_SIGN_SECP256K1_SHA256`. The signer computes Keccak-256 digests locally
/// and passes them to KMS in the `digest.sha256` field. KMS signs the raw 32-byte
/// digest without re-hashing, producing a valid Ethereum signature.
pub struct GcpKmsSigner {
    key_resource_name: String,
    address: Address,
    chain_id: Option<u64>,
    auth: Arc<dyn gcp_auth::TokenProvider>,
    client: reqwest::Client,
}

impl fmt::Debug for GcpKmsSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GcpKmsSigner")
            .field("key_resource_name", &self.key_resource_name)
            .field("address", &self.address)
            .finish()
    }
}

impl GcpKmsSigner {
    /// Creates a new GCP KMS signer.
    ///
    /// Fetches the public key from KMS and derives the Ethereum address.
    /// Uses Application Default Credentials for authentication.
    pub async fn new(key_resource_name: String) -> anyhow::Result<Self> {
        let auth = gcp_auth::provider()
            .await
            .map_err(|e| anyhow::anyhow!("failed to initialize GCP auth: {e}"))?;
        let client = reqwest::Client::new();

        let verifying_key =
            Self::fetch_public_key(auth.as_ref(), &client, &key_resource_name).await?;
        let address = address_from_public_key(&verifying_key);

        tracing::info!(
            %address,
            key_resource_name,
            "initialized GCP KMS signer",
        );

        Ok(Self {
            key_resource_name,
            address,
            chain_id: None,
            auth,
            client,
        })
    }

    async fn get_bearer_token(&self) -> anyhow::Result<String> {
        let token = self
            .auth
            .token(KMS_SCOPE)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get GCP auth token: {e}"))?;
        Ok(token.as_str().to_string())
    }

    async fn fetch_public_key(
        auth: &dyn gcp_auth::TokenProvider,
        client: &reqwest::Client,
        key_resource_name: &str,
    ) -> anyhow::Result<VerifyingKey> {
        let token = auth
            .token(KMS_SCOPE)
            .await
            .map_err(|e| anyhow::anyhow!("failed to get GCP auth token: {e}"))?;

        let url = format!(
            "https://cloudkms.googleapis.com/v1/{}/publicKey",
            key_resource_name,
        );

        let resp: serde_json::Value = client
            .get(&url)
            .bearer_auth(token.as_str())
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let pem = resp["pem"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("KMS response missing 'pem' field"))?;

        parse_secp256k1_pem(pem)
    }

    async fn sign_digest(&self, digest: &[u8; 32]) -> anyhow::Result<Vec<u8>> {
        let url = format!(
            "https://cloudkms.googleapis.com/v1/{}:asymmetricSign",
            self.key_resource_name,
        );

        let body = serde_json::json!({
            "digest": {
                "sha256": BASE64.encode(digest)
            }
        });

        let mut last_err = None;
        for attempt in 0..=KMS_SIGN_MAX_RETRIES {
            if attempt > 0 {
                let delay = KMS_SIGN_RETRY_BASE_DELAY * 2u32.pow(attempt - 1);
                tracing::warn!(
                    attempt,
                    max_retries = KMS_SIGN_MAX_RETRIES,
                    ?delay,
                    "retrying KMS sign request",
                );
                tokio::time::sleep(delay).await;
            }

            let token = match self.get_bearer_token().await {
                Ok(t) => t,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };

            let result = self
                .client
                .post(&url)
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await;

            let resp = match result {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(e.into());
                    continue;
                }
            };

            let resp = match resp.error_for_status() {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(e.into());
                    continue;
                }
            };

            let json: serde_json::Value = resp.json().await?;

            let sig_b64 = json["signature"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("KMS response missing 'signature' field"))?;

            return Ok(BASE64.decode(sig_b64)?);
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("KMS sign request failed")))
    }

    fn sign_hash_inner(&self, hash: &B256, der_bytes: &[u8]) -> alloy::signers::Result<Signature> {
        let k256_sig = K256Signature::from_der(der_bytes)
            .map_err(|e| alloy::signers::Error::Other(Box::new(e)))?;

        // Normalize s to low-s form as required by Ethereum (EIP-2).
        let k256_sig = k256_sig.normalize_s().unwrap_or(k256_sig);
        let (r_bytes, s_bytes) = k256_sig.split_bytes();

        let r = U256::from_be_slice(&r_bytes);
        let s = U256::from_be_slice(&s_bytes);

        // Determine recovery id by trying both parities.
        for v_parity in [false, true] {
            let sig = Signature::new(r, s, v_parity);
            if let Ok(recovered) = sig.recover_address_from_prehash(hash)
                && recovered == self.address
            {
                return Ok(sig);
            }
        }

        Err(alloy::signers::Error::Other(
            anyhow::anyhow!("failed to determine signature recovery id").into(),
        ))
    }
}

#[async_trait]
impl Signer for GcpKmsSigner {
    async fn sign_hash(&self, hash: &B256) -> alloy::signers::Result<Signature> {
        let digest: &[u8; 32] = hash.as_ref();
        let der_bytes = self
            .sign_digest(digest)
            .await
            .map_err(|e| alloy::signers::Error::Other(e.into()))?;

        self.sign_hash_inner(hash, &der_bytes)
    }

    fn address(&self) -> Address {
        self.address
    }

    fn chain_id(&self) -> Option<u64> {
        self.chain_id
    }

    fn set_chain_id(&mut self, chain_id: Option<u64>) {
        self.chain_id = chain_id;
    }
}

#[async_trait]
impl alloy::network::TxSigner<Signature> for GcpKmsSigner {
    fn address(&self) -> Address {
        self.address
    }

    async fn sign_transaction(
        &self,
        tx: &mut dyn SignableTransaction<Signature>,
    ) -> alloy::signers::Result<Signature> {
        if let Some(chain_id) = Signer::chain_id(self)
            && !tx.set_chain_id_checked(chain_id)
        {
            return Err(alloy::signers::Error::TransactionChainIdMismatch {
                signer: chain_id,
                tx: tx.chain_id().unwrap(),
            });
        }
        self.sign_hash(&tx.signature_hash()).await
    }
}

/// Derives an Ethereum address from a secp256k1 verifying (public) key.
fn address_from_public_key(key: &VerifyingKey) -> Address {
    let point = key.to_encoded_point(false);
    // Skip the 0x04 uncompressed prefix byte, hash the 64-byte x||y
    let hash = keccak256(&point.as_bytes()[1..]);
    Address::from_slice(&hash[12..])
}

/// Parses a PEM-encoded secp256k1 public key in SubjectPublicKeyInfo format.
fn parse_secp256k1_pem(pem: &str) -> anyhow::Result<VerifyingKey> {
    let k256_key = k256::PublicKey::from_public_key_pem(pem)
        .map_err(|e| anyhow::anyhow!("failed to parse secp256k1 PEM public key: {e}"))?;
    Ok(k256_key.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::pkcs8::EncodePublicKey;

    #[test]
    fn test_parse_secp256k1_pem() {
        let sk = SigningKey::from_slice(&[1u8; 32]).unwrap();
        let vk = *sk.verifying_key();

        let k256_pk = k256::PublicKey::from(vk);
        let pem = k256_pk
            .to_public_key_pem(k256::pkcs8::LineEnding::LF)
            .expect("failed to encode PEM");

        let parsed = parse_secp256k1_pem(&pem).expect("failed to parse PEM");
        assert_eq!(parsed, vk);

        let expected_address = address_from_public_key(&vk);
        let parsed_address = address_from_public_key(&parsed);
        assert_eq!(parsed_address, expected_address);
    }

    #[test]
    fn test_parse_secp256k1_pem_rejects_invalid() {
        let pem = "-----BEGIN PUBLIC KEY-----\naW52YWxpZA==\n-----END PUBLIC KEY-----";
        assert!(parse_secp256k1_pem(pem).is_err());
    }

    #[test]
    fn test_operator_signer_config_equality() {
        let key_bytes = [1u8; 32];
        let sk1 = SigningKey::from_slice(&key_bytes).unwrap();
        let sk2 = SigningKey::from_slice(&key_bytes).unwrap();

        let config1 = OperatorSignerConfig::Local(sk1);
        let config2 = OperatorSignerConfig::Local(sk2);
        assert_eq!(config1, config2);

        let kms1 = OperatorSignerConfig::GcpKms {
            resource_name: "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1"
                .to_string(),
        };
        let kms2 = OperatorSignerConfig::GcpKms {
            resource_name: "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/1"
                .to_string(),
        };
        assert_eq!(kms1, kms2);

        assert_ne!(config2, kms2);
    }
}
