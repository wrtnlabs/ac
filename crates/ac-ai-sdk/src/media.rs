//! Externalizing and re-inlining oversized media on UI message parts.
//!
//! A `file` part can carry its bytes inline as a `data:` URI. That is fine for
//! a thumbnail and ruinous for a screenshot: the payload is then duplicated
//! into every persisted copy of the conversation and re-sent on every model
//! step. The fix is content addressing — write the bytes once under a digest,
//! leave a store-relative reference in the part, and re-inline on the way back
//! to the model.
//!
//! Both directions are pure over `serde_json::Value` parts; storage is
//! injected, so a host chooses where blobs live and what a reference looks
//! like. [`RootedBlobStore`] is the ordinary filesystem binding.
//!
//! The two halves are deliberately asymmetric. Stripping is a durability
//! decision and propagates its errors: silently keeping a payload inline would
//! mean the conversation grows without bound. Inflating is a rendering
//! decision and degrades to blind: a blob that has gone missing should cost the
//! model one image, not fail the turn.

use std::io;
use std::path::{Component, Path, PathBuf};

use base64::Engine as _;
use base64::alphabet;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Payloads at or below this many decoded bytes stay inline; strictly larger
/// ones are externalized. A default, not a rule — [`StripConfig`] carries the
/// host's choice.
pub const DEFAULT_INLINE_THRESHOLD_BYTES: usize = 64 * 1024;

/// JavaScript's `Buffer.from(body, "base64")` tolerates missing padding and the
/// strict engine does not. A producer that trimmed padding is not malformed
/// input, so accept it. Whitespace is filtered before decoding.
const FORGIVING_B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Where externalized bytes go and what the part refers to them by.
pub struct StripConfig<'a> {
    /// Decoded size above which a payload is externalized.
    pub threshold_bytes: usize,
    /// Prefix for the generated reference, e.g. `"blobs/inline"`. The digest
    /// and an extension are appended, so the host owns the namespace and the
    /// content owns the name. Empty means the store root.
    pub reference_prefix: &'a str,
}

impl Default for StripConfig<'_> {
    fn default() -> Self {
        Self {
            threshold_bytes: DEFAULT_INLINE_THRESHOLD_BYTES,
            reference_prefix: "",
        }
    }
}

/// Externalize oversized inline images, in place.
///
/// For every `file` part whose `url` is a `data:image/*` URI decoding to more
/// than the configured threshold: digest the bytes, hand them to `write_blob`
/// under `<prefix>/<sha256>.<ext>`, and replace the part's `url` with that
/// reference. Non-`file` parts, non-image or malformed data URLs, and payloads
/// at or below the threshold pass through untouched.
///
/// Content addressing makes this idempotent: re-running over already-stripped
/// parts is a no-op, and two parts carrying identical bytes converge on one
/// blob.
pub fn strip_inline_media(
    parts: &mut [Value],
    config: &StripConfig<'_>,
    write_blob: &dyn Fn(&str, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    for part in parts.iter_mut() {
        let Some((url, _)) = file_part_fields(part) else {
            continue;
        };
        let Some((media_type, bytes)) = decode_image_data_url(url) else {
            continue;
        };
        if bytes.len() <= config.threshold_bytes {
            continue;
        }
        let reference = content_addressed_reference(&bytes, &media_type, config.reference_prefix);
        write_blob(&reference, &bytes)?;
        part["url"] = Value::String(reference);
    }
    Ok(())
}

/// Re-inline externalized images, in place — the inverse of
/// [`strip_inline_media`].
///
/// A `file` part whose `url` has no URI scheme is a store reference; its bytes
/// are read back and re-encoded as `data:<mediaType>;base64,…` so a replayed
/// message is visible to the model again. Scheme URLs (`data:`, `https:`, …)
/// are somebody else's and pass through. A missing or unreadable blob also
/// passes through: the model loses one image, which is strictly better than
/// failing a turn over a blob that is already gone.
pub fn inflate_media_references(parts: &mut [Value], read_blob: &dyn Fn(&str) -> Option<Vec<u8>>) {
    for part in parts.iter_mut() {
        let Some((url, media_type)) = file_part_fields(part) else {
            continue;
        };
        if !is_store_relative_reference(url) {
            continue;
        }
        let Some(bytes) = read_blob(url).filter(|b| !b.is_empty()) else {
            continue;
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let media_type = media_type.to_string();
        part["url"] = Value::String(format!("data:{media_type};base64,{encoded}"));
    }
}

/// A filesystem blob store contained under one root.
///
/// Every reference is resolved lexically AND re-checked against the root's
/// canonical path, so neither a `..` in the reference nor a symlinked ancestor
/// can place a write outside the root. Writes are skipped when the
/// content-addressed target already exists, which is what makes repeated
/// stripping cheap as well as idempotent.
#[derive(Clone, Debug)]
pub struct RootedBlobStore {
    root: PathBuf,
}

impl RootedBlobStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A `write_blob` binding for [`strip_inline_media`]. Blocking; call it
    /// from a blocking context.
    pub fn writer(&self) -> impl Fn(&str, &[u8]) -> io::Result<()> + use<> {
        let root = self.root.clone();
        move |reference, bytes| {
            let abs = contained_join(&root, reference)?;
            std::fs::create_dir_all(&root)?;
            let root_canon = root.canonicalize()?;
            if abs.exists() {
                let canon = abs.canonicalize()?;
                if !canon.starts_with(&root_canon) {
                    return Err(escape_error(reference));
                }
                return Ok(());
            }
            let parent = abs.parent().ok_or_else(|| escape_error(reference))?;
            std::fs::create_dir_all(parent)?;
            if !parent.canonicalize()?.starts_with(&root_canon) {
                return Err(escape_error(reference));
            }
            std::fs::write(&abs, bytes)
        }
    }

    /// A `read_blob` binding for [`inflate_media_references`]. Every failure —
    /// escape, missing file, io error — is `None`, which the caller treats as
    /// "leave the part alone".
    pub fn reader(&self) -> impl Fn(&str) -> Option<Vec<u8>> + use<> {
        let root = self.root.clone();
        move |reference| {
            let abs = contained_join(&root, reference).ok()?;
            let canon = abs.canonicalize().ok()?;
            if !canon.starts_with(root.canonicalize().ok()?) {
                return None;
            }
            std::fs::read(&canon).ok()
        }
    }
}

/// A stable on-disk extension for an image media type. Unknown types get
/// `bin` — the digest already owns the name, so the extension is a courtesy to
/// whoever opens the directory.
pub fn media_type_extension(media_type: &str) -> &'static str {
    match media_type.to_lowercase().trim() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        _ => "bin",
    }
}

fn content_addressed_reference(bytes: &[u8], media_type: &str, prefix: &str) -> String {
    let digest = Sha256::digest(bytes);
    let ext = media_type_extension(media_type);
    if prefix.is_empty() {
        format!("{digest:x}.{ext}")
    } else {
        format!("{}/{digest:x}.{ext}", prefix.trim_end_matches('/'))
    }
}

/// `(url, mediaType)` when the part is a `file` part carrying both as strings.
fn file_part_fields(part: &Value) -> Option<(&str, &str)> {
    if part.get("type").and_then(Value::as_str) != Some("file") {
        return None;
    }
    let url = part.get("url").and_then(Value::as_str)?;
    let media_type = part.get("mediaType").and_then(Value::as_str)?;
    Some((url, media_type))
}

/// A non-empty URL with no `^[a-z][a-z0-9+.-]*:` scheme — the exact shape
/// [`strip_inline_media`] writes.
fn is_store_relative_reference(url: &str) -> bool {
    if url.is_empty() {
        return false;
    }
    let Some(colon) = url.find(':') else {
        return true;
    };
    let prefix = &url[..colon];
    let mut chars = prefix.chars();
    let scheme = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'));
    !scheme
}

/// `(mediaType, bytes)` for a well-formed `data:image/…` URI, base64 or
/// percent-encoded. `None` for anything else, which callers leave untouched.
fn decode_image_data_url(url: &str) -> Option<(String, Vec<u8>)> {
    let rest = url.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let header = &rest[..comma];
    let body = &rest[comma + 1..];
    let semi = header.find(';');
    let media_type = semi.map_or(header, |i| &header[..i]).to_lowercase();
    if !media_type.starts_with("image/") {
        return None;
    }
    let params = semi.map_or(header, |i| &header[i + 1..]);
    let is_base64 = params.split(';').any(|p| p == "base64");
    let bytes = if is_base64 {
        let filtered: Vec<u8> = body.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        FORGIVING_B64.decode(filtered).ok()?
    } else {
        percent_decode_utf8(body)?
    };
    Some((media_type, bytes))
}

/// Percent-decode to bytes, requiring valid `%HH` sequences and a valid-UTF-8
/// result — matching `decodeURIComponent`, which throws on either.
fn percent_decode_utf8(body: &str) -> Option<Vec<u8>> {
    let src = body.as_bytes();
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        if src[i] == b'%' {
            let hex = src.get(i + 1..i + 3)?;
            let hi = (hex[0] as char).to_digit(16)?;
            let lo = (hex[1] as char).to_digit(16)?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(src[i]);
            i += 1;
        }
    }
    std::str::from_utf8(&out).ok()?;
    Some(out)
}

/// Lexically resolve `reference` inside `root`, rejecting absolute paths and
/// any `..` that would pop above the root.
fn contained_join(root: &Path, reference: &str) -> io::Result<PathBuf> {
    let rel = Path::new(reference);
    if rel.is_absolute() {
        return Err(escape_error(reference));
    }
    let mut out = root.to_path_buf();
    let mut depth = 0usize;
    for component in rel.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(seg) => {
                out.push(seg);
                depth += 1;
            }
            Component::ParentDir => {
                if depth == 0 {
                    return Err(escape_error(reference));
                }
                out.pop();
                depth -= 1;
            }
            Component::RootDir | Component::Prefix(_) => return Err(escape_error(reference)),
        }
    }
    Ok(out)
}

fn escape_error(reference: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("blob reference escapes the store root: {reference}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data_url(bytes: &[u8]) -> String {
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    }

    fn config() -> StripConfig<'static> {
        StripConfig {
            threshold_bytes: 8,
            reference_prefix: "blobs/inline",
        }
    }

    #[test]
    fn round_trips_an_oversized_image_through_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RootedBlobStore::new(tmp.path().to_path_buf());
        let bytes = vec![7u8; 64];
        let mut parts = vec![json!({
            "type": "file",
            "mediaType": "image/png",
            "url": data_url(&bytes),
        })];

        strip_inline_media(&mut parts, &config(), &store.writer()).unwrap();
        let reference = parts[0]["url"].as_str().unwrap().to_string();
        assert!(reference.starts_with("blobs/inline/"));
        assert!(reference.ends_with(".png"));
        assert_eq!(std::fs::read(tmp.path().join(&reference)).unwrap(), bytes);

        inflate_media_references(&mut parts, &store.reader());
        assert_eq!(parts[0]["url"].as_str().unwrap(), data_url(&bytes));
    }

    #[test]
    fn identical_bytes_converge_on_one_blob_and_stripping_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RootedBlobStore::new(tmp.path().to_path_buf());
        let bytes = vec![3u8; 64];
        let mut parts = vec![
            json!({ "type": "file", "mediaType": "image/png", "url": data_url(&bytes) }),
            json!({ "type": "file", "mediaType": "image/png", "url": data_url(&bytes) }),
        ];

        strip_inline_media(&mut parts, &config(), &store.writer()).unwrap();
        assert_eq!(parts[0]["url"], parts[1]["url"]);
        let before = parts[0]["url"].clone();

        // Re-running over already-stripped parts changes nothing.
        strip_inline_media(&mut parts, &config(), &store.writer()).unwrap();
        assert_eq!(parts[0]["url"], before);

        let files: Vec<_> = std::fs::read_dir(tmp.path().join("blobs/inline"))
            .unwrap()
            .collect();
        assert_eq!(files.len(), 1, "content addressing deduplicates");
    }

    #[test]
    fn at_threshold_and_non_image_parts_pass_through() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RootedBlobStore::new(tmp.path().to_path_buf());
        let exactly = data_url(&[1u8; 8]);
        let mut parts = vec![
            json!({ "type": "file", "mediaType": "image/png", "url": exactly }),
            json!({ "type": "text", "text": "not a file part" }),
            json!({ "type": "file", "mediaType": "application/pdf", "url": "data:application/pdf;base64,AAAA" }),
            json!({ "type": "file", "mediaType": "image/png", "url": "https://example.test/a.png" }),
        ];
        let before = parts.clone();
        strip_inline_media(&mut parts, &config(), &store.writer()).unwrap();
        assert_eq!(
            parts, before,
            "only oversized inline IMAGES are externalized"
        );
    }

    #[test]
    fn a_missing_blob_degrades_to_blind_rather_than_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RootedBlobStore::new(tmp.path().to_path_buf());
        let mut parts = vec![
            json!({ "type": "file", "mediaType": "image/png", "url": "blobs/inline/gone.png" }),
        ];
        inflate_media_references(&mut parts, &store.reader());
        assert_eq!(parts[0]["url"], "blobs/inline/gone.png");
    }

    #[test]
    fn a_reference_cannot_escape_the_store_root() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RootedBlobStore::new(tmp.path().join("root"));
        std::fs::create_dir_all(tmp.path().join("root")).unwrap();
        for escape in ["../outside.png", "/etc/passwd", "a/../../outside.png"] {
            assert!(
                store.writer()(escape, b"x").is_err(),
                "{escape} must be refused"
            );
            assert!(store.reader()(escape).is_none(), "{escape} must be refused");
        }
    }

    #[test]
    fn percent_encoded_bodies_and_unpadded_base64_both_decode() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RootedBlobStore::new(tmp.path().to_path_buf());
        let padded = base64::engine::general_purpose::STANDARD.encode([9u8; 10]);
        let mut parts = vec![
            json!({
                "type": "file", "mediaType": "image/png",
                "url": format!("data:image/png;base64,{}", padded.trim_end_matches('=')),
            }),
            json!({
                "type": "file", "mediaType": "image/svg+xml",
                "url": "data:image/svg+xml,%3Csvg%3E%3C%2Fsvg%3E%20padding%20to%20exceed",
            }),
        ];
        strip_inline_media(&mut parts, &config(), &store.writer()).unwrap();
        assert!(parts[0]["url"].as_str().unwrap().ends_with(".png"));
        assert!(parts[1]["url"].as_str().unwrap().ends_with(".svg"));
    }

    #[test]
    fn scheme_detection_distinguishes_a_reference_from_a_url() {
        assert!(is_store_relative_reference("blobs/inline/a.png"));
        // A colon inside a path segment is not a scheme.
        assert!(is_store_relative_reference("blobs/inline/x:y.png"));
        assert!(!is_store_relative_reference("data:image/png;base64,AA"));
        assert!(!is_store_relative_reference("blob:abc"));
        assert!(!is_store_relative_reference("HTTP:upper-scheme"));
        assert!(!is_store_relative_reference(""));
    }

    #[test]
    fn data_url_decoding_rejects_what_it_should() {
        let (media_type, bytes) = decode_image_data_url("data:image/svg+xml,%3Csvg%2F%3E").unwrap();
        assert_eq!(media_type, "image/svg+xml");
        assert_eq!(bytes, b"<svg/>");

        // A truncated percent escape is malformed, not best-effort.
        assert!(decode_image_data_url("data:image/svg+xml,%3").is_none());
        // No comma: no body.
        assert!(decode_image_data_url("data:image/png;base64").is_none());
        // Not an image.
        assert!(decode_image_data_url("data:text/plain;base64,AA==").is_none());
        // Missing padding is tolerated (JS producers strip it).
        assert_eq!(
            decode_image_data_url("data:image/png;base64,YWJjZA")
                .unwrap()
                .1,
            b"abcd"
        );
    }

    #[test]
    fn an_empty_prefix_writes_at_the_store_root() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RootedBlobStore::new(tmp.path().to_path_buf());
        // The extension comes from the DATA URL's own header — the bytes'
        // declared type — not the part's `mediaType` field, which a producer
        // can set independently.
        let encoded = base64::engine::general_purpose::STANDARD.encode([5u8; 64]);
        let mut parts = vec![json!({
            "type": "file",
            "mediaType": "image/png",
            "url": format!("data:image/gif;base64,{encoded}"),
        })];
        strip_inline_media(
            &mut parts,
            &StripConfig {
                threshold_bytes: 8,
                reference_prefix: "",
            },
            &store.writer(),
        )
        .unwrap();
        let reference = parts[0]["url"].as_str().unwrap();
        assert!(
            !reference.contains('/'),
            "{reference} should be a bare name"
        );
        assert!(reference.ends_with(".gif"));
    }
}
