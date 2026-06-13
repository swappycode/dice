//! Object storage behind the media-service. [`LocalFsStore`] is the dev
//! default: one file per object under a root dir, keyed by the media snowflake.
//! The [`MediaStore`] trait is the seam for a future S3 backend (SigV4 over the
//! workspace's ring-pinned reqwest — never `aws-sdk-s3`, which pulls
//! aws-lc-sys and trips `just gate-aws-lc`).

use std::path::PathBuf;

use async_trait::async_trait;
use bytes::Bytes;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("object not found")]
    NotFound,
    #[error("store io: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait MediaStore: Send + Sync {
    async fn put(&self, key: &str, data: &[u8]) -> Result<(), StoreError>;
    async fn get(&self, key: &str) -> Result<Bytes, StoreError>;
    async fn delete(&self, key: &str) -> Result<(), StoreError>;
}

/// Local filesystem store: `<root>/<key>`. Keys are decimal snowflakes (no path
/// separators possible), but we still reject any non-digit key as defence in
/// depth against path traversal.
pub struct LocalFsStore {
    root: PathBuf,
}

impl LocalFsStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, key: &str) -> Result<PathBuf, StoreError> {
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_digit()) {
            return Err(StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "media key must be a decimal id",
            )));
        }
        Ok(self.root.join(key))
    }
}

#[async_trait]
impl MediaStore for LocalFsStore {
    async fn put(&self, key: &str, data: &[u8]) -> Result<(), StoreError> {
        let path = self.path(key)?;
        tokio::fs::create_dir_all(&self.root).await?;
        tokio::fs::write(&path, data).await?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Bytes, StoreError> {
        let path = self.path(key)?;
        match tokio::fs::read(&path).await {
            Ok(v) => Ok(Bytes::from(v)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StoreError::NotFound),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        let path = self.path(key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Io(e)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn non_digit_keys_are_rejected() {
        let store = LocalFsStore::new("whatever");
        assert!(store.path("../etc/passwd").is_err());
        assert!(store.path("a/b").is_err());
        assert!(store.path("").is_err());
        assert!(store.path("123456789").is_ok());
    }
}
