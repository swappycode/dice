//! [`MediaService`]: the Postgres + object-store implementation of [`Media`].
//!
//! Upload validates (size, filename, content-type, image-sniff), writes the
//! bytes to the [`MediaStore`] FIRST, then the `media` metadata row — an
//! orphaned object is harmless garbage, whereas a row that 404s on read is a
//! broken attachment. A periodic GC sweep of unreferenced media is a future
//! hardening (orphaned objects on a failed-but-uncommitted upload, or media
//! left behind when a message is deleted).

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use dice_common::{MediaId, SnowflakeGenerator, UserId};
use sqlx::PgPool;

use crate::{IMAGE_TYPES, MAX_MEDIA_BYTES, Media, MediaError, MediaObject, MediaStore, StoreError};

const MAX_FILENAME_CHARS: usize = 255;
const MAX_CONTENT_TYPE_CHARS: usize = 255;

pub struct MediaService {
    pool: PgPool,
    store: Arc<dyn MediaStore>,
    ids: Arc<SnowflakeGenerator>,
    max_bytes: usize,
}

impl MediaService {
    pub fn new(pool: PgPool, store: Arc<dyn MediaStore>, ids: Arc<SnowflakeGenerator>) -> Self {
        Self {
            pool,
            store,
            ids,
            max_bytes: MAX_MEDIA_BYTES,
        }
    }

    /// Override the per-object size cap (tests use a small one).
    #[must_use]
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }
}

#[async_trait]
impl Media for MediaService {
    async fn upload(
        &self,
        uploader: UserId,
        filename: &str,
        content_type: &str,
        data: Bytes,
    ) -> Result<MediaObject, MediaError> {
        if data.is_empty() {
            return Err(MediaError::InvalidArgument("file is empty".to_owned()));
        }
        if data.len() > self.max_bytes {
            return Err(MediaError::TooLarge {
                max: self.max_bytes,
            });
        }
        let filename = sanitize_filename(filename)?;
        let content_type = validate_content_type(content_type)?;

        // Sniff dimensions for images (header-only, no full decode). A file that
        // claims an image type but won't parse is rejected as forged/corrupt.
        let (width, height) = if IMAGE_TYPES.contains(&content_type.as_str()) {
            match imagesize::blob_size(data.as_ref()) {
                Ok(dim) => (
                    u32::try_from(dim.width).unwrap_or(0),
                    u32::try_from(dim.height).unwrap_or(0),
                ),
                Err(_) => {
                    return Err(MediaError::InvalidArgument(
                        "declared an image type but the bytes are not a valid image".to_owned(),
                    ));
                }
            }
        } else {
            (0, 0)
        };

        let id = MediaId::from(self.ids.generate());
        let size_bytes = data.len() as u64;

        self.store
            .put(&id.raw().to_string(), data.as_ref())
            .await
            .map_err(store_internal)?;
        sqlx::query!(
            "INSERT INTO media (id, uploader_id, filename, content_type, size_bytes, width, height) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
            id.as_i64(),
            uploader.as_i64(),
            filename.as_str(),
            content_type.as_str(),
            size_bytes as i64,
            width as i32,
            height as i32,
        )
        .execute(&self.pool)
        .await
        .map_err(internal)?;

        Ok(MediaObject {
            id,
            uploader,
            filename,
            content_type,
            size_bytes,
            width,
            height,
        })
    }

    async fn read(&self, id: MediaId) -> Result<(MediaObject, Bytes), MediaError> {
        let row = sqlx::query!(
            r#"SELECT uploader_id, filename, content_type, size_bytes, width, height
               FROM media WHERE id = $1"#,
            id.as_i64()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?
        .ok_or(MediaError::NotFound)?;

        let bytes = match self.store.get(&id.raw().to_string()).await {
            Ok(b) => b,
            Err(StoreError::NotFound) => return Err(MediaError::NotFound),
            Err(e) => return Err(store_internal(e)),
        };

        let obj = MediaObject {
            id,
            uploader: UserId::from_i64(row.uploader_id),
            filename: row.filename,
            content_type: row.content_type,
            size_bytes: row.size_bytes as u64,
            width: row.width as u32,
            height: row.height as u32,
        };
        Ok((obj, bytes))
    }
}

/// Keep the basename only (drop any directory the client encoded), trim, and
/// enforce 1..=255 chars.
fn sanitize_filename(name: &str) -> Result<String, MediaError> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    let chars = base.chars().count();
    if chars == 0 {
        return Err(MediaError::InvalidArgument(
            "filename must not be empty".to_owned(),
        ));
    }
    if chars > MAX_FILENAME_CHARS {
        return Err(MediaError::InvalidArgument(format!(
            "filename exceeds {MAX_FILENAME_CHARS} characters"
        )));
    }
    Ok(base.to_owned())
}

/// Crude MIME shape check: ASCII `type/subtype`, both halves non-empty, 1..=255.
fn validate_content_type(ct: &str) -> Result<String, MediaError> {
    let ct = ct.trim();
    let chars = ct.chars().count();
    if chars == 0 || chars > MAX_CONTENT_TYPE_CHARS {
        return Err(MediaError::InvalidArgument(
            "content_type must be 1..=255 chars".to_owned(),
        ));
    }
    let well_formed = ct.is_ascii()
        && ct
            .split_once('/')
            .is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && !b.contains('/'));
    if !well_formed {
        return Err(MediaError::InvalidArgument(
            "content_type must be a MIME type (type/subtype)".to_owned(),
        ));
    }
    Ok(ct.to_owned())
}

fn internal<E>(e: E) -> MediaError
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let e = e.into();
    tracing::error!(error = %e, "media-service internal error");
    MediaError::Internal(e)
}

fn store_internal(e: StoreError) -> MediaError {
    internal(e)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn filename_keeps_basename_and_validates() {
        assert_eq!(sanitize_filename("  cat.png  ").unwrap(), "cat.png");
        assert_eq!(sanitize_filename("a/b/c/cat.png").unwrap(), "cat.png");
        assert_eq!(sanitize_filename(r"C:\Users\x\cat.png").unwrap(), "cat.png");
        assert!(sanitize_filename("   ").is_err());
        assert!(sanitize_filename(&"x".repeat(256)).is_err());
    }

    #[test]
    fn content_type_shape_is_checked() {
        assert_eq!(validate_content_type(" image/png ").unwrap(), "image/png");
        assert!(validate_content_type("notamime").is_err());
        assert!(validate_content_type("/png").is_err());
        assert!(validate_content_type("image/").is_err());
        assert!(validate_content_type("imàge/png").is_err()); // non-ASCII
    }
}
