use base64::Engine;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};
use tracing::{debug, warn};

use crate::llm::provider::{ChatMessage, LlmProvider, MessageContent};

const DEFAULT_MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

pub fn normalize_messages_for_provider(messages: &mut [ChatMessage], provider: &dyn LlmProvider) {
    let max_image_bytes = provider
        .image_constraints()
        .map(|c| c.max_image_bytes)
        .unwrap_or(DEFAULT_MAX_IMAGE_BYTES);

    if max_image_bytes == 0 {
        return;
    }

    let mut resized = 0usize;
    let mut omitted = 0usize;

    for message in messages {
        let MessageContent::Parts(parts) = &mut message.content else {
            continue;
        };

        let mut normalized = Vec::with_capacity(parts.len());
        for mut part in std::mem::take(parts) {
            let Some(image_b64) = part.image_base64.as_ref() else {
                normalized.push(part);
                continue;
            };
            if estimate_base64_decoded_len(image_b64) <= max_image_bytes {
                normalized.push(part);
                continue;
            }

            let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(image_b64) else {
                warn!("Dropping invalid base64 image payload during normalization");
                omitted += 1;
                continue;
            };
            if decoded.len() <= max_image_bytes {
                normalized.push(part);
                continue;
            }

            match resize_and_compress_to_limit(&decoded, max_image_bytes) {
                Some(out) => {
                    part.part_type = "image".to_string();
                    part.text = None;
                    part.image_url = None;
                    part.media_type = Some("image/jpeg".to_string());
                    part.image_base64 = Some(base64::engine::general_purpose::STANDARD.encode(out));
                    resized += 1;
                    normalized.push(part);
                }
                None => {
                    omitted += 1;
                }
            }
        }
        *parts = normalized;
    }

    if resized > 0 || omitted > 0 {
        debug!(
            provider = provider.id(),
            max_image_bytes,
            resized,
            omitted,
            "Applied shared image normalization for multimodal LLM input"
        );
    }
}

fn estimate_base64_decoded_len(data: &str) -> usize {
    let trimmed = data.trim();
    if trimmed.is_empty() {
        return 0;
    }
    let padding = trimmed
        .as_bytes()
        .iter()
        .rev()
        .take_while(|&&b| b == b'=')
        .count();
    (trimmed.len() / 4) * 3usize - padding.min(2)
}

fn resize_and_compress_to_limit(image_bytes: &[u8], max_bytes: usize) -> Option<Vec<u8>> {
    let source = image::load_from_memory(image_bytes).ok()?;
    let (width, height) = source.dimensions();
    if width == 0 || height == 0 {
        return None;
    }

    let scales = [1.0f32, 0.9, 0.8, 0.7, 0.6, 0.5, 0.4, 0.3, 0.2];
    let qualities = [85u8, 75, 65, 55, 45, 35];

    for scale in scales {
        let candidate = scaled_copy(&source, width, height, scale);
        for quality in qualities {
            let Some(encoded) = encode_jpeg(&candidate, quality) else {
                continue;
            };
            if encoded.len() <= max_bytes {
                return Some(encoded);
            }
        }
    }

    None
}

fn scaled_copy(source: &DynamicImage, width: u32, height: u32, scale: f32) -> DynamicImage {
    if (scale - 1.0).abs() < f32::EPSILON {
        return source.clone();
    }
    let next_w = ((width as f32 * scale).round() as u32).max(1);
    let next_h = ((height as f32 * scale).round() as u32).max(1);
    source.resize_exact(next_w, next_h, FilterType::Lanczos3)
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, quality);
    encoder.encode_image(image).ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::provider::{
        ChatMessage, ContentPart, ImageConstraints, LlmProvider, LlmResponse, StreamChunk, ToolSpec,
    };
    use anyhow::Result;
    use async_trait::async_trait;
    use std::io::Cursor;

    struct TestProvider;
    struct TinyLimitProvider;

    #[async_trait]
    impl LlmProvider for TestProvider {
        fn id(&self) -> &str {
            "test"
        }

        fn image_constraints(&self) -> Option<ImageConstraints> {
            Some(ImageConstraints {
                max_image_bytes: 50_000,
            })
        }

        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
            _temperature: f32,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse::Text(String::new()))
        }

        async fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
            _temperature: f32,
        ) -> Result<crate::llm::provider::ChunkStream> {
            Ok(Box::pin(futures::stream::iter(vec![Ok(StreamChunk::Done)])))
        }
    }

    #[async_trait]
    impl LlmProvider for TinyLimitProvider {
        fn id(&self) -> &str {
            "tiny"
        }

        fn image_constraints(&self) -> Option<ImageConstraints> {
            Some(ImageConstraints { max_image_bytes: 1 })
        }

        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
            _temperature: f32,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse::Text(String::new()))
        }

        async fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolSpec],
            _temperature: f32,
        ) -> Result<crate::llm::provider::ChunkStream> {
            Ok(Box::pin(futures::stream::iter(vec![Ok(StreamChunk::Done)])))
        }
    }

    #[test]
    fn normalizes_oversized_images_to_fit_limit() {
        let mut img = image::RgbImage::new(1400, 1000);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            *pixel = image::Rgb([
                ((x * 31 + y * 7) % 255) as u8,
                ((x * 17 + y * 13) % 255) as u8,
                ((x * 11 + y * 29) % 255) as u8,
            ]);
        }

        let dynamic = DynamicImage::ImageRgb8(img);
        let mut raw_png = Cursor::new(Vec::new());
        dynamic
            .write_to(&mut raw_png, image::ImageFormat::Png)
            .expect("encode png");
        let raw_png = raw_png.into_inner();
        assert!(raw_png.len() > 50_000);

        let mut messages = vec![ChatMessage {
            role: "user".to_string(),
            content: MessageContent::Parts(vec![ContentPart::image_base64(
                "image/png",
                base64::engine::general_purpose::STANDARD.encode(raw_png),
            )]),
            tool_call_id: None,
            tool_calls: vec![],
            is_error: false,
        }];

        normalize_messages_for_provider(&mut messages, &TestProvider);

        let MessageContent::Parts(parts) = &messages[0].content else {
            panic!("expected parts");
        };
        let payload = parts[0]
            .image_base64
            .as_ref()
            .expect("expected image payload after normalization");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .expect("decode payload");
        assert!(decoded.len() <= 50_000);
        assert_eq!(parts[0].media_type.as_deref(), Some("image/jpeg"));
    }

    #[test]
    fn drops_invalid_image_payload_without_adding_text_tokens() {
        let mut messages = vec![ChatMessage {
            role: "user".to_string(),
            content: MessageContent::Parts(vec![
                ContentPart::text("hello"),
                ContentPart::image_base64("image/png", "not-base64"),
            ]),
            tool_call_id: None,
            tool_calls: vec![],
            is_error: false,
        }];

        normalize_messages_for_provider(&mut messages, &TinyLimitProvider);

        let MessageContent::Parts(parts) = &messages[0].content else {
            panic!("expected parts");
        };
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].part_type, "text");
        assert_eq!(parts[0].text.as_deref(), Some("hello"));
    }
}
