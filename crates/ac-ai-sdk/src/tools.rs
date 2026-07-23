//! Bin-side host additions: the `image_gen` host tool (registered beside the
//! built-ins when `--image-gen` is set) and the resolver behind the
//! `GET /api/files/{*path}` endpoint. Both go through the same [`PathPolicy`]
//! containment the built-in file tools use — the workspace stays the only
//! subtree either can touch.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ac_tool::{Capability, PathPolicy, Tool, ToolCtx, ToolOutput};
use base64::Engine as _;
use futures::future::BoxFuture;
use serde::Deserialize;

/// Maximum decoded image size `image_gen` will save.
const IMAGE_CAP: usize = 10 * 1024 * 1024;

/// Maximum file size the `/api/files` endpoint will serve.
pub const SERVE_CAP: u64 = 20 * 1024 * 1024;

const GENERATION_TIMEOUT: Duration = Duration::from_secs(120);

type Generate = Arc<
    dyn Fn(String) -> BoxFuture<'static, Result<serde_json::Value, String>> + Send + Sync + 'static,
>;

/// Generate an image from a text prompt and save it into the workspace.
///
/// The image is written under `generated/` with a host-chosen name; the model
/// only supplies the prompt, never the path.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct ImageGenInput {
    /// Text description of the image to generate.
    pub prompt: String,
    /// Optional aspect ratio of the image.
    pub aspect: Option<Aspect>,
}

#[derive(Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Aspect {
    Square,
    Landscape,
    Portrait,
}

impl Aspect {
    fn hint(self) -> &'static str {
        match self {
            Aspect::Square => " The image should be square (1:1).",
            Aspect::Landscape => " The image should be in wide landscape orientation (16:9).",
            Aspect::Portrait => " The image should be in tall portrait orientation (9:16).",
        }
    }
}

/// Generates an image via an image-capable chat model and saves it to disk.
pub struct ImageGen {
    generate: Generate,
}

impl ImageGen {
    pub fn new(api_key: String, model: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(GENERATION_TIMEOUT)
            .build()
            .expect("reqwest client");
        Self {
            generate: Arc::new(move |prompt: String| {
                let client = client.clone();
                let api_key = api_key.clone();
                let model = model.clone();
                Box::pin(async move {
                    let body = serde_json::json!({
                        "model": model,
                        "messages": [{ "role": "user", "content": prompt }],
                        "modalities": ["image", "text"],
                    });
                    let response = client
                        .post("https://openrouter.ai/api/v1/chat/completions")
                        .header("Authorization", format!("Bearer {api_key}"))
                        .json(&body)
                        .send()
                        .await
                        .map_err(|e| format!("image generation request failed: {e}"))?;
                    let status = response.status();
                    let text = response
                        .text()
                        .await
                        .map_err(|e| format!("failed to read image generation response: {e}"))?;
                    if !status.is_success() {
                        return Err(format!(
                            "image generation failed: HTTP {status}: {}",
                            snippet(&text)
                        ));
                    }
                    serde_json::from_str(&text)
                        .map_err(|e| format!("image generation returned invalid JSON: {e}"))
                })
            }),
        }
    }

    #[cfg(test)]
    fn with_generate(generate: Generate) -> Self {
        Self { generate }
    }
}

impl Tool for ImageGen {
    type Input = ImageGenInput;

    fn name(&self) -> &'static str {
        "image_gen"
    }

    fn description(&self) -> String {
        "Generate an image from a text prompt and save it into the workspace \
         under generated/. Returns a JSON object {path, mimeType, bytes} with \
         the saved path; use that path when referring to the image. Optional \
         aspect: square, landscape, or portrait."
            .into()
    }

    fn capability(&self) -> Capability {
        Capability::Mutating
    }

    fn run(
        self: Arc<Self>,
        input: Self::Input,
        ctx: Arc<ToolCtx>,
    ) -> BoxFuture<'static, ToolOutput> {
        Box::pin(async move {
            let mut prompt = input.prompt.trim().to_string();
            if prompt.is_empty() {
                return ToolOutput::error("prompt must not be empty");
            }
            if let Some(aspect) = input.aspect {
                prompt.push_str(aspect.hint());
            }

            let response = match (self.generate)(prompt).await {
                Ok(v) => v,
                Err(e) => return ToolOutput::error(e),
            };
            let url = match extract_image_url(&response) {
                Ok(u) => u,
                Err(e) => return ToolOutput::error(e),
            };
            let image = match decode_data_url(url) {
                Ok(i) => i,
                Err(e) => return ToolOutput::error(e),
            };

            // The relative path is host-generated — the model (and thus the
            // prompt) has no influence over where the file lands.
            let rel = format!(
                "generated/img-{}.{}",
                uuid::Uuid::new_v4().simple(),
                image.ext
            );
            let resolved = match ctx.policy.resolve_write(Path::new(&rel)) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            if let Some(parent) = resolved.parent()
                && let Err(e) = tokio::fs::create_dir_all(parent).await
            {
                return ToolOutput::error(format!("cannot create parent dirs: {e}"));
            }
            if let Err(e) = tokio::fs::write(&resolved, &image.bytes).await {
                return ToolOutput::error(format!("cannot write {rel}: {e}"));
            }
            if let Ok(meta) = tokio::fs::metadata(&resolved).await
                && let Ok(mtime) = meta.modified()
            {
                ctx.file_times.stamp(resolved, mtime);
            }

            ToolOutput::ok(
                serde_json::json!({
                    "path": rel,
                    "mimeType": image.mime,
                    "bytes": image.bytes.len(),
                })
                .to_string(),
            )
        })
    }
}

struct DecodedImage {
    mime: &'static str,
    ext: &'static str,
    bytes: Vec<u8>,
}

fn extract_image_url(response: &serde_json::Value) -> Result<&str, String> {
    let message = &response["choices"][0]["message"];
    if let Some(url) = message["images"][0]["image_url"]["url"].as_str() {
        return Ok(url);
    }
    // No image: the model refused or answered in text. Surface that text so
    // the calling model can adapt the prompt.
    match assistant_text(message) {
        Some(text) => Err(format!(
            "the model returned no image; it said: {}",
            snippet(&text)
        )),
        None => Err("the model returned no image and no text".to_string()),
    }
}

fn assistant_text(message: &serde_json::Value) -> Option<String> {
    match &message["content"] {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Array(parts) => {
            let text: Vec<&str> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
            (!text.is_empty()).then(|| text.join(" "))
        }
        _ => None,
    }
}

fn decode_data_url(url: &str) -> Result<DecodedImage, String> {
    let rest = url
        .strip_prefix("data:")
        .ok_or_else(|| format!("expected a data: URL, got: {}", snippet(url)))?;
    let (mime, payload) = rest
        .split_once(";base64,")
        .ok_or_else(|| "image data URL is not base64-encoded".to_string())?;
    let (mime, ext) = match mime {
        "image/png" => ("image/png", "png"),
        "image/jpeg" => ("image/jpeg", "jpg"),
        "image/webp" => ("image/webp", "webp"),
        other => return Err(format!("unsupported image mime type: {}", snippet(other))),
    };
    // Refuse before decoding: 4 base64 chars encode 3 bytes.
    if payload.len() > IMAGE_CAP / 3 * 4 + 4 {
        return Err(format!("generated image exceeds {IMAGE_CAP} bytes"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| format!("invalid base64 image payload: {e}"))?;
    if bytes.len() > IMAGE_CAP {
        return Err(format!("generated image exceeds {IMAGE_CAP} bytes"));
    }
    Ok(DecodedImage { mime, ext, bytes })
}

fn snippet(text: &str) -> String {
    const CAP: usize = 400;
    if text.chars().count() <= CAP {
        text.to_string()
    } else {
        let cut: String = text.chars().take(CAP).collect();
        format!("{cut}…")
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ServeError {
    NotFound,
    Forbidden,
    TooLarge,
}

#[derive(Debug)]
pub struct ServedFile {
    pub path: PathBuf,
    pub content_type: &'static str,
}

/// Resolve a `/api/files/{*path}` URL path to a servable file. Containment is
/// judged by the same policy the tools write through — `..` and symlink
/// escapes are refused, not just missing.
pub fn resolve_served_file(
    policy: &dyn PathPolicy,
    url_path: &str,
) -> Result<ServedFile, ServeError> {
    let resolved = policy
        .resolve_read(Path::new(url_path))
        .map_err(|_| ServeError::Forbidden)?;
    let meta = std::fs::metadata(&resolved).map_err(|_| ServeError::NotFound)?;
    if meta.is_dir() {
        return Err(ServeError::NotFound);
    }
    if meta.len() > SERVE_CAP {
        return Err(ServeError::TooLarge);
    }
    Ok(ServedFile {
        path: resolved,
        content_type: content_type_for(url_path),
    })
}

pub fn content_type_for(path: &str) -> &'static str {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("txt") | Some("md") => "text/plain; charset=utf-8",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_tool::SubtreePolicy;

    fn ctx(root: &Path) -> Arc<ToolCtx> {
        Arc::new(ToolCtx::new(Arc::new(SubtreePolicy::new(root).unwrap())))
    }

    fn stub(response: serde_json::Value) -> Arc<ImageGen> {
        Arc::new(ImageGen::with_generate(Arc::new(move |_prompt| {
            let response = response.clone();
            Box::pin(async move { Ok(response) })
        })))
    }

    fn image_response(data_url: &str) -> serde_json::Value {
        serde_json::json!({
            "choices": [{
                "message": {
                    "images": [{ "image_url": { "url": data_url } }],
                }
            }]
        })
    }

    fn png_data_url(bytes: &[u8]) -> String {
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }

    #[tokio::test]
    async fn stubbed_image_lands_on_disk_and_the_path_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let payload = b"not-really-a-png".to_vec();
        let tool = stub(image_response(&png_data_url(&payload)));

        let out = tool
            .run(
                ImageGenInput {
                    prompt: "a red circle".into(),
                    aspect: Some(Aspect::Landscape),
                },
                ctx(dir.path()),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);

        let result: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        let rel = result["path"].as_str().unwrap();
        assert!(rel.starts_with("generated/img-"), "{rel}");
        assert!(rel.ends_with(".png"), "{rel}");
        assert_eq!(result["mimeType"], "image/png");
        assert_eq!(result["bytes"], payload.len());
        // The base64 never reaches the model — only the short JSON envelope.
        assert!(out.content.len() < 200);

        // The returned relative path round-trips through the policy to the
        // decoded bytes on disk.
        let on_disk = std::fs::read(dir.path().join(rel)).unwrap();
        assert_eq!(on_disk, payload);
    }

    #[tokio::test]
    async fn prompt_content_cannot_influence_the_save_path() {
        let dir = tempfile::tempdir().unwrap();
        let tool = stub(image_response(&png_data_url(b"x")));

        let out = tool
            .run(
                ImageGenInput {
                    prompt: "../../outside/evil".into(),
                    aspect: None,
                },
                ctx(dir.path()),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);

        let result: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        let rel = result["path"].as_str().unwrap();
        // The name is host-generated: always directly under generated/, no
        // traversal components regardless of prompt content.
        assert!(rel.starts_with("generated/img-"), "{rel}");
        let saved = dir.path().canonicalize().unwrap().join(rel);
        assert_eq!(
            saved.parent().unwrap(),
            dir.path().canonicalize().unwrap().join("generated")
        );
        assert!(saved.exists());
    }

    #[tokio::test]
    async fn missing_images_surfaces_the_assistant_text() {
        let dir = tempfile::tempdir().unwrap();
        let tool = stub(serde_json::json!({
            "choices": [{
                "message": { "content": "I can only describe, not draw." }
            }]
        }));

        let out = tool
            .run(
                ImageGenInput {
                    prompt: "a red circle".into(),
                    aspect: None,
                },
                ctx(dir.path()),
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("no image"), "{}", out.content);
        assert!(
            out.content.contains("I can only describe, not draw."),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn bad_base64_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = stub(image_response("data:image/png;base64,%%%not-base64%%%"));

        let out = tool
            .run(
                ImageGenInput {
                    prompt: "a red circle".into(),
                    aspect: None,
                },
                ctx(dir.path()),
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("base64"), "{}", out.content);
    }

    #[tokio::test]
    async fn oversized_image_is_refused_before_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let oversized = format!(
            "data:image/png;base64,{}",
            "A".repeat(IMAGE_CAP / 3 * 4 + 8)
        );
        let tool = stub(image_response(&oversized));

        let out = tool
            .run(
                ImageGenInput {
                    prompt: "a red circle".into(),
                    aspect: None,
                },
                ctx(dir.path()),
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("exceeds"), "{}", out.content);
    }

    #[test]
    fn unsupported_mime_is_refused() {
        assert!(decode_data_url("data:application/pdf;base64,QUJD").is_err());
        assert!(decode_data_url("https://example.com/img.png").is_err());
    }

    #[test]
    fn serving_refuses_containment_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SubtreePolicy::new(dir.path()).unwrap();

        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s").unwrap();

        assert_eq!(
            resolve_served_file(&policy, "../secret.txt").unwrap_err(),
            ServeError::Forbidden
        );
        assert_eq!(
            resolve_served_file(&policy, outside.path().join("secret.txt").to_str().unwrap())
                .unwrap_err(),
            ServeError::Forbidden
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
            assert_eq!(
                resolve_served_file(&policy, "link/secret.txt").unwrap_err(),
                ServeError::Forbidden
            );
        }
    }

    #[test]
    fn serving_resolves_contained_files_and_404s_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SubtreePolicy::new(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("generated")).unwrap();
        std::fs::write(dir.path().join("generated/a.png"), b"png").unwrap();

        let served = resolve_served_file(&policy, "generated/a.png").unwrap();
        assert_eq!(served.content_type, "image/png");
        assert_eq!(std::fs::read(&served.path).unwrap(), b"png");

        assert_eq!(
            resolve_served_file(&policy, "generated/missing.png").unwrap_err(),
            ServeError::NotFound
        );
        assert_eq!(
            resolve_served_file(&policy, "generated").unwrap_err(),
            ServeError::NotFound
        );
    }

    #[test]
    fn serving_caps_size() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SubtreePolicy::new(dir.path()).unwrap();
        let big = dir.path().join("big.bin");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(SERVE_CAP + 1).unwrap();
        assert_eq!(
            resolve_served_file(&policy, "big.bin").unwrap_err(),
            ServeError::TooLarge
        );
    }

    #[test]
    fn extension_mapping_is_case_insensitive_and_defaults() {
        assert_eq!(content_type_for("a.PNG"), "image/png");
        assert_eq!(content_type_for("a.jpg"), "image/jpeg");
        assert_eq!(content_type_for("a.jpeg"), "image/jpeg");
        assert_eq!(content_type_for("a.webp"), "image/webp");
        assert_eq!(content_type_for("a.gif"), "image/gif");
        assert_eq!(content_type_for("a.svg"), "image/svg+xml");
        assert_eq!(content_type_for("a.txt"), "text/plain; charset=utf-8");
        assert_eq!(content_type_for("notes.md"), "text/plain; charset=utf-8");
        assert_eq!(content_type_for("a.json"), "application/json");
        assert_eq!(content_type_for("a.bin"), "application/octet-stream");
        assert_eq!(content_type_for("no-extension"), "application/octet-stream");
    }
}
