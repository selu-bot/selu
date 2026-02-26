/// gRPC client proxy for capability containers.
///
/// The generated tonic client lives here, along with a thin wrapper
/// that provides the orchestrator-facing `invoke` / `stream_invoke` API.
/// Credentials are injected here before the call is forwarded to the container.

// ── Generated tonic code ──────────────────────────────────────────────────────
pub mod selu_capability {
    tonic::include_proto!("selu.capability.v1");
}

use anyhow::{anyhow, Result};
use futures::StreamExt;
use serde_json::Value;
use tonic::transport::Channel;
use tracing::debug;

use selu_capability::capability_client::CapabilityClient;
use selu_capability::{InvokeRequest, InvokeChunk};

/// A connected gRPC client to one running capability container.
/// Cheap to clone — the underlying channel uses connection pooling internally.
#[derive(Clone)]
pub struct CapabilityGrpcClient {
    channel: Channel,
    capability_id: String,
}

impl CapabilityGrpcClient {
    /// Connect to a container by its local gRPC port.
    pub async fn connect(grpc_port: u16, capability_id: impl Into<String>) -> Result<Self> {
        let addr = format!("http://127.0.0.1:{}", grpc_port);
        let channel = Channel::from_shared(addr)
            .map_err(|e| anyhow!("Invalid gRPC address: {}", e))?
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
    ///
    /// Returns the JSON result string to be fed back to the LLM.
    pub async fn invoke(
        &self,
        tool_name: &str,
        args: Value,
        credentials: Value,
        session_id: &str,
    ) -> Result<String> {
        let mut client = CapabilityClient::new(self.channel.clone());

        let request = InvokeRequest {
            tool_name: tool_name.to_string(),
            args_json: serde_json::to_vec(&args)?.into(),
            config_json: serde_json::to_vec(&credentials)?.into(),
            session_id: session_id.to_string(),
            capability_id: self.capability_id.clone(),
        };

        debug!(
            capability = %self.capability_id,
            tool = %tool_name,
            "Invoking capability tool"
        );

        let response = client
            .invoke(request)
            .await
            .map_err(|e| anyhow!("gRPC invoke failed: {}", e))?
            .into_inner();

        if !response.error.is_empty() {
            return Err(anyhow!("Capability '{}' tool '{}' returned error: {}", self.capability_id, tool_name, response.error));
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
    ) -> Result<impl futures::Stream<Item = Result<String>>> {
        let mut client = CapabilityClient::new(self.channel.clone());

        let request = InvokeRequest {
            tool_name: tool_name.to_string(),
            args_json: serde_json::to_vec(&args)?.into(),
            config_json: serde_json::to_vec(&credentials)?.into(),
            session_id: session_id.to_string(),
            capability_id: self.capability_id.clone(),
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

        let mapped = stream.map(|result: std::result::Result<InvokeChunk, tonic::Status>| {
            match result {
                Err(e) => Err(anyhow!("gRPC stream error: {}", e)),
                Ok(chunk) if !chunk.error.is_empty() => {
                    Err(anyhow!("Capability stream error: {}", chunk.error))
                }
                Ok(chunk) => {
                    String::from_utf8(chunk.data.to_vec())
                        .map_err(|_| anyhow!("Non-UTF8 data in stream chunk"))
                }
            }
        });

        Ok(mapped)
    }
}
