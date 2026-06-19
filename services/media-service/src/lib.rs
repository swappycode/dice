//! media-service: upload + serve message attachments (and, later, avatars).
//!
//! The [`Media`] trait is the BINDING contract consumed by api-gateway (the
//! `/v1/media` REST routes) and the monolith. Bytes live behind a
//! [`MediaStore`] — [`LocalFsStore`] in dev; an S3/rustls backend (SigV4 over
//! the workspace's ring-pinned reqwest, never `aws-sdk-s3`) is the documented
//! seam, MinIO deferred. Metadata rows live in Postgres (`media` table).
//!
//! Ownership split: media-service owns upload/serve and the `media` row.
//! chat-service owns the `message_attachments` junction and references media
//! ids at send time (validating uploader + one-shot use in the send tx), so
//! there is no chat→media trait dependency — both just share the database.

use bytes::Bytes;
use dice_common::{MediaId, UserId};
use dice_protocol::v1;

pub mod gc;
mod service;
mod store;

pub use service::MediaService;
pub use store::{LocalFsStore, MediaStore, StoreError};

/// Hard cap on a single uploaded object (8 MiB in dev). api-gateway sizes its
/// `/v1/media` request-body limit to match this, and the service re-checks.
pub const MAX_MEDIA_BYTES: usize = 8 * 1024 * 1024;

/// Image MIME types we sniff for `width`/`height` at upload. A file declaring
/// one of these must actually parse as that image (forgery/corruption guard).
pub(crate) const IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("media not found")]
    NotFound,
    #[error("file exceeds the {max}-byte limit")]
    TooLarge { max: usize },
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("internal media error")]
    Internal(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// Stored object metadata (mirrors a `media` row). Converts to the wire
/// [`v1::Attachment`] carried on messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaObject {
    pub id: MediaId,
    pub uploader: UserId,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: u64,
    pub width: u32,
    pub height: u32,
}

impl MediaObject {
    pub fn to_attachment(&self) -> v1::Attachment {
        v1::Attachment {
            id: self.id.raw(),
            filename: self.filename.clone(),
            content_type: self.content_type.clone(),
            size_bytes: self.size_bytes,
            width: self.width,
            height: self.height,
        }
    }
}

#[async_trait::async_trait]
pub trait Media: Send + Sync {
    /// Validate, store the bytes, and persist metadata. The returned object's
    /// id is what a client passes in `SendMessageRequest.attachment_ids`.
    async fn upload(
        &self,
        uploader: UserId,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<MediaObject, MediaError>;

    /// Metadata + raw bytes for serving `GET /v1/media/{id}`.
    async fn read(&self, id: MediaId) -> Result<(MediaObject, Bytes), MediaError>;
}
