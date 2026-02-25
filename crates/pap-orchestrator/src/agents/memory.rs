/// Semantic memory: stores and retrieves message embeddings for RAG-style context.
///
/// Uses an LLM provider's embedding endpoint (OpenAI-compatible) to generate
/// vector embeddings for messages. Stores them in SQLite as raw f32 bytes.
/// Retrieval uses cosine similarity computed in Rust.
///
/// The memory store is optional — agents with `memory.policy = "none"` skip it.
use anyhow::{anyhow, Context, Result};
use ring::digest::{digest, SHA256};
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use sqlx::SqlitePool;
use tracing::{debug, warn};
use uuid::Uuid;

/// Configuration for the embedding API.
#[derive(Debug, Clone)]
pub struct EmbeddingConfig {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: "https://api.openai.com".into(),
            model: "text-embedding-3-small".into(),
        }
    }
}

pub struct MemoryStore {
    db: SqlitePool,
    client: Client,
    config: EmbeddingConfig,
}

impl MemoryStore {
    pub fn new(db: SqlitePool, config: EmbeddingConfig) -> Self {
        Self {
            db,
            client: Client::new(),
            config,
        }
    }

    /// Generate an embedding for the given text via the OpenAI-compatible API.
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        if self.config.api_key.is_empty() {
            return Err(anyhow!("No embedding API key configured"));
        }

        let body = json!({
            "model": self.config.model,
            "input": text,
        });

        let resp = self
            .client
            .post(format!("{}/v1/embeddings", self.config.base_url))
            .bearer_auth(&self.config.api_key)
            .json(&body)
            .send()
            .await
            .context("Failed to call embedding API")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Embedding API error {}: {}", status, text));
        }

        #[derive(Deserialize)]
        struct EmbData {
            embedding: Vec<f32>,
        }
        #[derive(Deserialize)]
        struct EmbResp {
            data: Vec<EmbData>,
        }

        let parsed: EmbResp = resp.json().await.context("Invalid embedding response")?;
        parsed
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| anyhow!("No embedding data in response"))
    }

    /// Store an embedding for a message. Skips if already embedded (by content hash).
    pub async fn store_message(
        &self,
        message_id: &str,
        session_id: &str,
        pipe_id: &str,
        content: &str,
    ) -> Result<()> {
        let content_hash = hex_sha256(content);

        // Check if already embedded
        let existing = sqlx::query!(
            "SELECT id FROM message_embeddings WHERE content_hash = ? AND session_id = ?",
            content_hash,
            session_id
        )
        .fetch_optional(&self.db)
        .await?;

        if existing.is_some() {
            debug!(message_id = %message_id, "Embedding already exists, skipping");
            return Ok(());
        }

        let embedding = match self.embed(content).await {
            Ok(e) => e,
            Err(e) => {
                warn!("Failed to generate embedding: {e}");
                return Ok(()); // Non-fatal: memory is best-effort
            }
        };

        let dimension = embedding.len() as i64;
        let embedding_bytes = floats_to_bytes(&embedding);
        let id = Uuid::new_v4().to_string();

        sqlx::query!(
            r#"INSERT INTO message_embeddings
               (id, message_id, session_id, pipe_id, content_hash, embedding, dimension)
               VALUES (?, ?, ?, ?, ?, ?, ?)"#,
            id,
            message_id,
            session_id,
            pipe_id,
            content_hash,
            embedding_bytes,
            dimension,
        )
        .execute(&self.db)
        .await
        .context("Failed to store embedding")?;

        debug!(message_id = %message_id, dim = dimension, "Stored message embedding");
        Ok(())
    }

    /// Retrieve the top-k most relevant messages for a query, using cosine similarity.
    /// Returns (message_id, content, similarity_score) tuples.
    pub async fn retrieve(
        &self,
        query: &str,
        session_id: &str,
        top_k: u32,
    ) -> Result<Vec<(String, String, f32)>> {
        let query_embedding = self.embed(query).await?;

        // Load all embeddings for this session
        let rows = sqlx::query!(
            r#"SELECT me.message_id, me.embedding, me.dimension, m.content
               FROM message_embeddings me
               JOIN messages m ON m.id = me.message_id
               WHERE me.session_id = ?"#,
            session_id
        )
        .fetch_all(&self.db)
        .await?;

        if rows.is_empty() {
            return Ok(Vec::new());
        }

        // Compute cosine similarity for each
        let mut scored: Vec<(String, String, f32)> = rows
            .into_iter()
            .filter_map(|row| {
                let stored = bytes_to_floats(&row.embedding);
                if stored.len() != query_embedding.len() {
                    return None; // Dimension mismatch
                }
                let sim = cosine_similarity(&query_embedding, &stored);
                Some((row.message_id, row.content, sim))
            })
            .collect();

        // Sort by similarity descending
        scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k as usize);

        Ok(scored)
    }
}

// ── Vector math helpers ──────────────────────────────────────────────────────

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a * norm_b)
}

fn floats_to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn bytes_to_floats(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn hex_sha256(input: &str) -> String {
    let d = digest(&SHA256, input.as_bytes());
    d.as_ref()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_cosine_orthogonal() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn test_float_roundtrip() {
        let original = vec![1.5f32, -2.0, 3.14, 0.0];
        let bytes = floats_to_bytes(&original);
        let recovered = bytes_to_floats(&bytes);
        assert_eq!(original, recovered);
    }
}
