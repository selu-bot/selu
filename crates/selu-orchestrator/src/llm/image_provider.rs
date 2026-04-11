use anyhow::{Result, anyhow};
use async_trait::async_trait;
use base64::Engine;
use reqwest::Client;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Clone)]
pub struct ImageInput {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct GeneratedImage {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[async_trait]
pub trait ImageProvider: Send + Sync {
    async fn generate(&self, prompt: &str) -> Result<GeneratedImage>;
    async fn edit(&self, prompt: &str, inputs: &[ImageInput]) -> Result<GeneratedImage>;
}

pub struct OpenAiCompatibleImageProvider {
    client: Client,
    api_key: String,
    base_url: String,
    model_id: String,
    api_flavor: ImageApiFlavor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageApiFlavor {
    OpenAi,
    Xai,
}

impl OpenAiCompatibleImageProvider {
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model_id: impl Into<String>,
        api_flavor: ImageApiFlavor,
    ) -> Self {
        Self {
            client: Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .pool_max_idle_per_host(4)
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .build()
                .expect("Failed to build HTTP client"),
            api_key: api_key.into(),
            base_url: base_url.into(),
            model_id: model_id.into(),
            api_flavor,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ImageApiResponse {
    data: Vec<ImageApiDatum>,
}

#[derive(Debug, Deserialize)]
struct ImageApiDatum {
    b64_json: Option<String>,
    url: Option<String>,
}

#[async_trait]
impl ImageProvider for OpenAiCompatibleImageProvider {
    async fn generate(&self, prompt: &str) -> Result<GeneratedImage> {
        let mut body = json!({
            "model": self.model_id,
            "prompt": prompt,
            "response_format": "b64_json"
        });
        if self.api_flavor == ImageApiFlavor::OpenAi {
            body["size"] = json!("1024x1024");
        }

        let parsed: ImageApiResponse = loop {
            let resp = self
                .client
                .post(format!("{}/v1/images/generations", self.base_url))
                .bearer_auth(&self.api_key)
                .timeout(std::time::Duration::from_secs(120))
                .json(&body)
                .send()
                .await?;

            if resp.status().is_success() {
                break resp.json().await?;
            }

            let status = resp.status();
            let error_body = resp.text().await.unwrap_or_default();
            if self.api_flavor == ImageApiFlavor::OpenAi
                && status.as_u16() == 400
                && error_body.contains("Argument not supported: size")
                && body.get("size").is_some()
            {
                if let Some(obj) = body.as_object_mut() {
                    obj.remove("size");
                }
                continue;
            }

            return Err(anyhow!(
                "image generation failed ({}): {}",
                status,
                error_body
            ));
        };

        let data = parsed
            .data
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("image generation response missing data block"))?;
        let bytes = if let Some(b64) = data.b64_json {
            base64::engine::general_purpose::STANDARD.decode(b64)?
        } else if let Some(url) = data.url {
            let resp = self.client.get(url).send().await?;
            resp.bytes().await?.to_vec()
        } else {
            return Err(anyhow!("image generation response missing image data"));
        };

        Ok(GeneratedImage {
            filename: "generated-image.png".to_string(),
            mime_type: "image/png".to_string(),
            data: bytes,
        })
    }

    async fn edit(&self, prompt: &str, inputs: &[ImageInput]) -> Result<GeneratedImage> {
        if inputs.is_empty() {
            return Err(anyhow!("image edit requires at least one input image"));
        }

        if self.api_flavor == ImageApiFlavor::Xai {
            let mut images = Vec::new();
            for input in inputs.iter().take(5) {
                let data_uri = format!(
                    "data:{};base64,{}",
                    input.mime_type,
                    base64::engine::general_purpose::STANDARD.encode(&input.data)
                );
                images.push(json!({
                    "type": "image_url",
                    "url": data_uri
                }));
            }

            let mut body = json!({
                "model": self.model_id,
                "prompt": prompt,
                "response_format": "b64_json"
            });
            if images.len() == 1 {
                body["image"] = images[0].clone();
            } else {
                body["images"] = json!(images);
            }

            let resp = self
                .client
                .post(format!("{}/v1/images/edits", self.base_url))
                .bearer_auth(&self.api_key)
                .timeout(std::time::Duration::from_secs(120))
                .json(&body)
                .send()
                .await?;

            if !resp.status().is_success() {
                let status = resp.status();
                let error_body = resp.text().await.unwrap_or_default();
                return Err(anyhow!("image edit failed ({}): {}", status, error_body));
            }

            let parsed: ImageApiResponse = resp.json().await?;
            let data = parsed
                .data
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("image edit response missing data block"))?;
            let bytes = if let Some(b64) = data.b64_json {
                base64::engine::general_purpose::STANDARD.decode(b64)?
            } else if let Some(url) = data.url {
                let resp = self.client.get(url).send().await?;
                resp.bytes().await?.to_vec()
            } else {
                return Err(anyhow!("image edit response missing image data"));
            };

            return Ok(GeneratedImage {
                filename: "edited-image.png".to_string(),
                mime_type: "image/png".to_string(),
                data: bytes,
            });
        }

        let mut include_size = true;
        let parsed: ImageApiResponse = loop {
            let mut form = Form::new()
                .text("model", self.model_id.clone())
                .text("prompt", prompt.to_string())
                .text("response_format", "b64_json".to_string());
            if include_size {
                form = form.text("size", "1024x1024".to_string());
            }

            for input in inputs.iter().take(4) {
                let part = Part::bytes(input.data.clone())
                    .file_name(input.filename.clone())
                    .mime_str(&input.mime_type)?;
                form = form.part("image[]", part);
            }

            let resp = self
                .client
                .post(format!("{}/v1/images/edits", self.base_url))
                .bearer_auth(&self.api_key)
                .timeout(std::time::Duration::from_secs(120))
                .multipart(form)
                .send()
                .await?;

            let status = resp.status();
            if status.is_success() {
                break resp.json().await?;
            }
            let error_body = resp.text().await.unwrap_or_default();
            if include_size
                && status.as_u16() == 400
                && error_body.contains("Argument not supported: size")
            {
                include_size = false;
                continue;
            }
            return Err(anyhow!("image edit failed ({}): {}", status, error_body));
        };

        let data = parsed
            .data
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("image edit response missing data block"))?;
        let bytes = if let Some(b64) = data.b64_json {
            base64::engine::general_purpose::STANDARD.decode(b64)?
        } else if let Some(url) = data.url {
            let resp = self.client.get(url).send().await?;
            resp.bytes().await?.to_vec()
        } else {
            return Err(anyhow!("image edit response missing image data"));
        };

        Ok(GeneratedImage {
            filename: "edited-image.png".to_string(),
            mime_type: "image/png".to_string(),
            data: bytes,
        })
    }
}
