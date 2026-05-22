use std::path::PathBuf;

use open_sandbox_contracts::error::AgentError;

pub struct ImageManager {
    _root_dir: PathBuf,
}

impl ImageManager {
    pub fn new(_root_dir: PathBuf) -> Self {
        todo!()
    }

    pub async fn pull_and_unpack(&self, _image_ref: &str) -> Result<PathBuf, AgentError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pull_alpine_returns_rootfs_path() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = ImageManager::new(dir.path().to_path_buf());

        let rootfs = mgr.pull_and_unpack("alpine:latest").await.unwrap();

        assert!(rootfs.exists());
        assert!(rootfs.join("bin/sh").exists());
    }

    #[tokio::test]
    async fn pull_caches_on_second_call() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = ImageManager::new(dir.path().to_path_buf());

        let first = mgr.pull_and_unpack("alpine:latest").await.unwrap();
        let second = mgr.pull_and_unpack("alpine:latest").await.unwrap();

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn pull_nonexistent_image_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = ImageManager::new(dir.path().to_path_buf());

        let result = mgr
            .pull_and_unpack("nonexistent-registry.invalid/nosuchimage:v999")
            .await;

        assert!(result.is_err());
    }
}
