/// Encrypted credential storage.
///
/// All secret values are encrypted at rest with AES-256-GCM using a
/// per-instance key stored in the `SELU_ENCRYPTION_KEY` env var (base64, 32 bytes).
///
/// Layout of an encrypted blob (base64-encoded in the DB):
///   [12-byte nonce][ciphertext + 16-byte GCM tag]
///
/// Two scopes:
///   - `user`   → `user_credentials` table: per-user, per-capability
///   - `system` → `system_credentials` table: shared across all users
use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use ring::aead::{
    self, AES_256_GCM, BoundKey, NONCE_LEN, Nonce, NonceSequence, OpeningKey, SealingKey,
    UnboundKey,
};
use ring::error::Unspecified;
use ring::rand::{SecureRandom, SystemRandom};
use sqlx::SqlitePool;
use uuid::Uuid;

// ── Encryption helpers ────────────────────────────────────────────────────────

struct OneNonce([u8; NONCE_LEN]);

impl NonceSequence for OneNonce {
    fn advance(&mut self) -> std::result::Result<Nonce, Unspecified> {
        Ok(Nonce::assume_unique_for_key(self.0))
    }
}

/// Encrypt `plaintext` with AES-256-GCM.  
/// Returns base64(nonce || ciphertext+tag).
pub fn encrypt(key_bytes: &[u8; 32], plaintext: &[u8]) -> Result<String> {
    let rng = SystemRandom::new();
    let mut nonce_bytes = [0u8; NONCE_LEN];
    rng.fill(&mut nonce_bytes)
        .map_err(|_| anyhow!("RNG failed"))?;

    let unbound =
        UnboundKey::new(&AES_256_GCM, key_bytes).map_err(|_| anyhow!("Invalid AES key"))?;
    let mut sealing = SealingKey::new(unbound, OneNonce(nonce_bytes));

    let mut in_out = plaintext.to_vec();
    sealing
        .seal_in_place_append_tag(aead::Aad::empty(), &mut in_out)
        .map_err(|_| anyhow!("Encryption failed"))?;

    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&in_out);
    Ok(B64.encode(&blob))
}

/// Decrypt a base64 blob produced by `encrypt`.
pub fn decrypt(key_bytes: &[u8; 32], blob_b64: &str) -> Result<Vec<u8>> {
    let blob = B64
        .decode(blob_b64)
        .context("Invalid base64 in credential blob")?;
    if blob.len() < NONCE_LEN {
        return Err(anyhow!("Credential blob too short"));
    }
    let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
    let nonce_arr: [u8; NONCE_LEN] = nonce_bytes.try_into().unwrap();

    let unbound =
        UnboundKey::new(&AES_256_GCM, key_bytes).map_err(|_| anyhow!("Invalid AES key"))?;
    let mut opening = OpeningKey::new(unbound, OneNonce(nonce_arr));

    let mut in_out = ciphertext.to_vec();
    let plaintext = opening
        .open_in_place(aead::Aad::empty(), &mut in_out)
        .map_err(|_| anyhow!("Decryption failed — wrong key or corrupted blob"))?;

    Ok(plaintext.to_vec())
}

/// Parse a base64-encoded 32-byte key from the app config.
pub fn parse_key(base64_key: &str) -> Result<[u8; 32]> {
    let raw = B64
        .decode(base64_key.trim())
        .context("SELU_ENCRYPTION_KEY is not valid base64")?;
    raw.try_into()
        .map_err(|_| anyhow!("SELU_ENCRYPTION_KEY must decode to exactly 32 bytes"))
}

// ── CredentialStore ───────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct CredentialStore {
    db: SqlitePool,
    key: [u8; 32],
}

impl CredentialStore {
    pub fn new(db: SqlitePool, key: [u8; 32]) -> Self {
        Self { db, key }
    }

    /// Decrypt a raw encrypted blob using this store's key.
    /// Used by the LLM registry to decrypt API keys from the llm_providers table.
    pub fn decrypt_raw(&self, blob_b64: &str) -> Result<Vec<u8>> {
        decrypt(&self.key, blob_b64)
    }

    /// Encrypt a raw value using this store's key.
    /// Used for encrypting LLM provider API keys before storing.
    #[allow(dead_code)]
    pub fn encrypt_raw(&self, plaintext: &[u8]) -> Result<String> {
        encrypt(&self.key, plaintext)
    }

    // ── User-scoped ───────────────────────────────────────────────────────────

    /// Store (or replace) a user credential.
    pub async fn set_user(
        &self,
        user_id: &str,
        capability_id: &str,
        credential_name: &str,
        value: &str,
    ) -> Result<()> {
        let encrypted = encrypt(&self.key, value.as_bytes())?;
        let id = Uuid::new_v4().to_string();

        sqlx::query!(
            r#"INSERT INTO user_credentials
               (id, user_id, capability_id, credential_name, encrypted_value)
               VALUES (?, ?, ?, ?, ?)
               ON CONFLICT(user_id, capability_id, credential_name)
               DO UPDATE SET encrypted_value = excluded.encrypted_value,
                             created_at = datetime('now')"#,
            id,
            user_id,
            capability_id,
            credential_name,
            encrypted
        )
        .execute(&self.db)
        .await
        .context("Failed to store user credential")?;

        Ok(())
    }

    /// Retrieve a user credential's plaintext value, or `None` if not set.
    pub async fn get_user(
        &self,
        user_id: &str,
        capability_id: &str,
        credential_name: &str,
    ) -> Result<Option<String>> {
        let row = sqlx::query!(
            "SELECT encrypted_value FROM user_credentials
             WHERE user_id = ? AND capability_id = ? AND credential_name = ?
               AND (expires_at IS NULL OR expires_at > datetime('now'))",
            user_id,
            capability_id,
            credential_name
        )
        .fetch_optional(&self.db)
        .await
        .context("DB error reading user credential")?;

        match row {
            None => Ok(None),
            Some(r) => {
                let plain = decrypt(&self.key, &r.encrypted_value)?;
                Ok(Some(
                    String::from_utf8(plain).context("Credential is not valid UTF-8")?,
                ))
            }
        }
    }

    /// Delete a user credential.
    pub async fn delete_user(
        &self,
        user_id: &str,
        capability_id: &str,
        credential_name: &str,
    ) -> Result<()> {
        sqlx::query!(
            "DELETE FROM user_credentials
             WHERE user_id = ? AND capability_id = ? AND credential_name = ?",
            user_id,
            capability_id,
            credential_name
        )
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// List all credential names stored for (user, capability).
    pub async fn list_user(&self, user_id: &str, capability_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query!(
            "SELECT credential_name FROM user_credentials
             WHERE user_id = ? AND capability_id = ?
               AND (expires_at IS NULL OR expires_at > datetime('now'))",
            user_id,
            capability_id
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(|r| r.credential_name).collect())
    }

    // ── System-scoped ─────────────────────────────────────────────────────────

    /// Store (or replace) a system credential.
    pub async fn set_system(
        &self,
        capability_id: &str,
        credential_name: &str,
        value: &str,
    ) -> Result<()> {
        let encrypted = encrypt(&self.key, value.as_bytes())?;
        let id = Uuid::new_v4().to_string();

        sqlx::query!(
            r#"INSERT INTO system_credentials
               (id, capability_id, credential_name, encrypted_value)
               VALUES (?, ?, ?, ?)
               ON CONFLICT(capability_id, credential_name)
               DO UPDATE SET encrypted_value = excluded.encrypted_value,
                             created_at = datetime('now')"#,
            id,
            capability_id,
            credential_name,
            encrypted
        )
        .execute(&self.db)
        .await
        .context("Failed to store system credential")?;

        Ok(())
    }

    /// Retrieve a system credential's plaintext value, or `None` if not set.
    pub async fn get_system(
        &self,
        capability_id: &str,
        credential_name: &str,
    ) -> Result<Option<String>> {
        let row = sqlx::query!(
            "SELECT encrypted_value FROM system_credentials
             WHERE capability_id = ? AND credential_name = ?",
            capability_id,
            credential_name
        )
        .fetch_optional(&self.db)
        .await
        .context("DB error reading system credential")?;

        match row {
            None => Ok(None),
            Some(r) => {
                let plain = decrypt(&self.key, &r.encrypted_value)?;
                Ok(Some(
                    String::from_utf8(plain).context("Credential is not valid UTF-8")?,
                ))
            }
        }
    }

    /// Delete a system credential.
    pub async fn delete_system(&self, capability_id: &str, credential_name: &str) -> Result<()> {
        sqlx::query!(
            "DELETE FROM system_credentials
             WHERE capability_id = ? AND credential_name = ?",
            capability_id,
            credential_name
        )
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// List all credential names stored for a capability (system scope).
    pub async fn list_system(&self, capability_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query!(
            "SELECT credential_name FROM system_credentials WHERE capability_id = ?",
            capability_id
        )
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(|r| r.credential_name).collect())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        [0x42u8; 32]
    }

    #[test]
    fn round_trip() {
        let key = test_key();
        let plaintext = b"super-secret-token";
        let blob = encrypt(&key, plaintext).unwrap();
        let recovered = decrypt(&key, &blob).unwrap();
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn different_nonces_each_time() {
        let key = test_key();
        let a = encrypt(&key, b"hello").unwrap();
        let b = encrypt(&key, b"hello").unwrap();
        // Nonces are random so blobs must differ even for equal plaintexts
        assert_ne!(a, b);
    }

    #[test]
    fn wrong_key_fails() {
        let key_a = test_key();
        let key_b = [0xFFu8; 32];
        let blob = encrypt(&key_a, b"secret").unwrap();
        assert!(decrypt(&key_b, &blob).is_err());
    }

    #[test]
    fn parse_key_rejects_short() {
        // 16-byte key encoded is too short
        let short = B64.encode([0u8; 16]);
        assert!(parse_key(&short).is_err());
    }

    #[test]
    fn parse_key_accepts_32_bytes() {
        let valid = B64.encode([0u8; 32]);
        assert!(parse_key(&valid).is_ok());
    }
}
