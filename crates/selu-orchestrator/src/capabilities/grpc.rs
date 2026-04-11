/// gRPC client proxy for capability containers.
///
/// The generated tonic client lives here, along with a thin wrapper
/// that provides the orchestrator-facing `invoke` / `stream_invoke` API.
/// Credentials are injected here before the call is forwarded to the container.

// ── Generated tonic code ──────────────────────────────────────────────────────
pub mod selu_capability {
    tonic::include_proto!("selu.capability.v1");
}

use anyhow::{Result, anyhow};
use futures::StreamExt;
use serde_json::Value;
use std::time::Duration;
use tonic::transport::Channel;
use tracing::{debug, warn};

use selu_capability::capability_client::CapabilityClient;
use selu_capability::{
    ArtifactChunk, DownloadOutputArtifactRequest, InvokeChunk, InvokeRequest,
    UploadInputArtifactChunk, UploadInputArtifactResponse,
};

/// A connected gRPC client to one running capability container.
/// Cheap to clone — the underlying channel uses connection pooling internally.
#[derive(Clone)]
pub struct CapabilityGrpcClient {
    channel: Channel,
    capability_id: String,
}

/// Timeout for a single gRPC tool invocation (2 minutes).
/// Playwright's navigation timeout is 30s, so this gives plenty of headroom
/// for slower tools while preventing indefinite hangs.
const INVOKE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_GRPC_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Timeout for establishing the initial gRPC channel connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

impl CapabilityGrpcClient {
    /// Connect to a container by its gRPC address (e.g. `http://127.0.0.1:55001`).
    pub async fn connect(grpc_addr: &str, capability_id: impl Into<String>) -> Result<Self> {
        let channel = Channel::from_shared(grpc_addr.to_string())
            .map_err(|e| anyhow!("Invalid gRPC address: {}", e))?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(INVOKE_TIMEOUT)
            .connect()
            .await
            .map_err(|e| anyhow!("Failed to connect to capability gRPC: {}", e))?;

        Ok(Self {
            channel,
            capability_id: capability_id.into(),
        })
    }

    /// Synchronous tool invocation.
    ///
    /// `tool_name`    — name as declared in the capability manifest  
    /// `args`         — JSON object matching the tool's input_schema  
    /// `credentials`  — key/value map of declared credentials, injected as config_json  
    /// `session_id`   — forwarded to the container so Environment capabilities can  
    ///                  locate their workspace volume  
    /// `thread_id`    — conversation thread within the session; capabilities use this  
    ///                  to isolate per-thread state (e.g. separate browser tabs)  
    ///
    /// Returns the JSON result string to be fed back to the LLM.
    pub async fn invoke(
        &self,
        tool_name: &str,
        args: Value,
        credentials: Value,
        session_id: &str,
        thread_id: &str,
    ) -> Result<String> {
        let mut client = CapabilityClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES);

        let request = InvokeRequest {
            tool_name: tool_name.to_string(),
            args_json: serde_json::to_vec(&args)?.into(),
            config_json: serde_json::to_vec(&credentials)?.into(),
            session_id: session_id.to_string(),
            capability_id: self.capability_id.clone(),
            thread_id: thread_id.to_string(),
        };

        debug!(
            capability = %self.capability_id,
            tool = %tool_name,
            "Invoking capability tool"
        );

        let response = client
            .invoke(request)
            .await
            .map_err(|e| {
                warn!(
                    capability = %self.capability_id,
                    tool = %tool_name,
                    "gRPC invoke failed: {}", e
                );
                anyhow!("gRPC invoke failed: {}", e)
            })?
            .into_inner();

        if !response.error.is_empty() {
            warn!(
                capability = %self.capability_id,
                tool = %tool_name,
                "Capability returned error: {}", response.error
            );
            return Err(anyhow!(
                "Capability '{}' tool '{}' returned error: {}",
                self.capability_id,
                tool_name,
                response.error
            ));
        }

        let result = String::from_utf8(response.result_json.to_vec())
            .map_err(|_| anyhow!("Capability returned non-UTF8 result_json"))?;

        debug!(
            capability = %self.capability_id,
            tool = %tool_name,
            "Tool invocation complete"
        );

        Ok(result)
    }

    /// Streaming invocation for Environment-class capabilities.
    ///
    /// Returns an async stream of string chunks. The caller should
    /// concatenate them to build the full result.
    #[allow(dead_code)]
    pub async fn stream_invoke(
        &self,
        tool_name: &str,
        args: Value,
        credentials: Value,
        session_id: &str,
        thread_id: &str,
    ) -> Result<impl futures::Stream<Item = Result<String>>> {
        let mut client = CapabilityClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES);

        let request = InvokeRequest {
            tool_name: tool_name.to_string(),
            args_json: serde_json::to_vec(&args)?.into(),
            config_json: serde_json::to_vec(&credentials)?.into(),
            session_id: session_id.to_string(),
            capability_id: self.capability_id.clone(),
            thread_id: thread_id.to_string(),
        };

        debug!(
            capability = %self.capability_id,
            tool = %tool_name,
            "Starting streaming capability invocation"
        );

        let stream = client
            .stream_invoke(request)
            .await
            .map_err(|e| anyhow!("gRPC stream_invoke failed: {}", e))?
            .into_inner();

        let mapped =
            stream.map(
                |result: std::result::Result<InvokeChunk, tonic::Status>| match result {
                    Err(e) => Err(anyhow!("gRPC stream error: {}", e)),
                    Ok(chunk) if !chunk.error.is_empty() => {
                        Err(anyhow!("Capability stream error: {}", chunk.error))
                    }
                    Ok(chunk) => String::from_utf8(chunk.data.to_vec())
                        .map_err(|_| anyhow!("Non-UTF8 data in stream chunk")),
                },
            );

        Ok(mapped)
    }

    pub async fn upload_input_artifact(
        &self,
        filename: &str,
        mime_type: &str,
        data: &[u8],
    ) -> Result<String> {
        let mut client = CapabilityClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES);

        let chunk_size = 256 * 1024;
        let mut chunks: Vec<UploadInputArtifactChunk> = Vec::new();
        let mut offset = 0usize;
        while offset < data.len() {
            let end = (offset + chunk_size).min(data.len());
            chunks.push(UploadInputArtifactChunk {
                filename: if offset == 0 {
                    filename.to_string()
                } else {
                    String::new()
                },
                mime_type: if offset == 0 {
                    mime_type.to_string()
                } else {
                    String::new()
                },
                data: data[offset..end].to_vec(),
            });
            offset = end;
        }
        if chunks.is_empty() {
            chunks.push(UploadInputArtifactChunk {
                filename: filename.to_string(),
                mime_type: mime_type.to_string(),
                data: Vec::new(),
            });
        }

        let response: UploadInputArtifactResponse = client
            .upload_input_artifact(tokio_stream::iter(chunks))
            .await
            .map_err(|e| anyhow!("gRPC upload_input_artifact failed: {}", e))?
            .into_inner();

        if !response.error.is_empty() {
            return Err(anyhow!(
                "Capability artifact upload failed: {}",
                response.error
            ));
        }
        if response.capability_artifact_id.is_empty() {
            return Err(anyhow!("Capability returned empty capability_artifact_id"));
        }
        Ok(response.capability_artifact_id)
    }

    pub async fn download_output_artifact(&self, capability_artifact_id: &str) -> Result<Vec<u8>> {
        let mut client = CapabilityClient::new(self.channel.clone())
            .max_decoding_message_size(MAX_GRPC_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_GRPC_MESSAGE_BYTES);

        let mut stream = client
            .download_output_artifact(DownloadOutputArtifactRequest {
                capability_artifact_id: capability_artifact_id.to_string(),
            })
            .await
            .map_err(|e| anyhow!("gRPC download_output_artifact failed: {}", e))?
            .into_inner();

        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            let chunk: ArtifactChunk =
                item.map_err(|e| anyhow!("gRPC artifact stream error: {}", e))?;
            if !chunk.error.is_empty() {
                return Err(anyhow!(
                    "Capability artifact stream failed: {}",
                    chunk.error
                ));
            }
            out.extend_from_slice(&chunk.data);
            if chunk.done {
                break;
            }
        }
        if out.is_empty() {
            return Err(anyhow!(
                "Capability artifact '{}' returned no data",
                capability_artifact_id
            ));
        }
        Ok(out)
    }
}
