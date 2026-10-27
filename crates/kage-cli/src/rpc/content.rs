//! ACP prompt blocks as engine content and back.

use kage_acp::acp::{BlobContent, ContentBlock, ResourceLink, TextContent};
use kage_core::{Content, MediaSource};

/// An image or clip held in `source` as an ACP block: `kind` wraps
/// its bytes, a link becomes a resource link, and a missing blob a line
/// saying so.
pub(super) fn source_block(
    source: &MediaSource,
    mime: &str,
    kind: fn(BlobContent) -> ContentBlock,
) -> ContentBlock {
    match source {
        MediaSource::Base64 { data } => kind(BlobContent {
            data: data.clone(),
            mime_type: mime.to_owned(),
            uri: None,
        }),
        MediaSource::Url { url } => ContentBlock::ResourceLink(ResourceLink {
            uri: url.clone(),
            name: url.clone(),
            mime_type: Some(mime.to_owned()),
        }),
        MediaSource::Blob { .. } => ContentBlock::Text(TextContent {
            text: source.missing_note(mime).unwrap_or_default(),
        }),
    }
}

/// One ACP prompt block as the content the engine receives. Embedded
/// text becomes a resource block, images and audio clips stay media, and
/// what the model cannot take becomes one line saying what was attached.
pub(super) fn prompt_content(block: ContentBlock) -> Content {
    let image = |data: String, mime: String| Content::Image {
        source: MediaSource::Base64 { data },
        mime,
    };
    let audio = |data: String, mime: String| Content::Audio {
        source: MediaSource::Base64 { data },
        mime,
    };
    let text = |text: String| Content::Text { text };
    match block {
        ContentBlock::Text(t) => text(t.text),
        ContentBlock::Image(blob) => image(blob.data, blob.mime_type),
        ContentBlock::Audio(blob) => audio(blob.data, blob.mime_type),
        ContentBlock::ResourceLink(link) => text(match link.uri.strip_prefix("file://") {
            Some(path) => format!("Referenced file: {path}"),
            None => format!("Referenced resource: {} ({})", link.uri, link.name),
        }),
        ContentBlock::Resource(embedded) => {
            let field = |key: &str| {
                embedded
                    .resource
                    .get(key)
                    .and_then(serde_json::Value::as_str)
            };
            let uri = field("uri").unwrap_or_default();
            let mime = field("mimeType");
            match (field("text"), field("blob"), mime) {
                (Some(body), ..) => text(kage_core::resource_block::render(uri, None, mime, body)),
                (None, Some(data), Some(mime)) if mime.starts_with("image/") => {
                    image(data.to_owned(), mime.to_owned())
                }
                (None, Some(data), Some(mime)) if mime.starts_with("audio/") => {
                    audio(data.to_owned(), mime.to_owned())
                }
                (None, Some(_), _) => text(format!(
                    "[binary resource {uri}: {}]",
                    mime.unwrap_or("application/octet-stream")
                )),
                (None, None, _) => text("[resource omitted]".to_owned()),
            }
        }
        ContentBlock::Unknown => text("[unsupported block omitted]".to_owned()),
    }
}
