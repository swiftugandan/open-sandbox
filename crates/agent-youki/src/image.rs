use std::path::PathBuf;

use open_sandbox_contracts::error::AgentError;

use futures_util::StreamExt;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::{Client, Reference};

pub struct ImageManager {
    root_dir: PathBuf,
    client: Client,
}

impl ImageManager {
    pub fn new(root_dir: PathBuf) -> Self {
        let config = ClientConfig {
            protocol: ClientProtocol::Https,
            ..Default::default()
        };
        let client = Client::new(config);
        Self { root_dir, client }
    }

    pub async fn pull_and_unpack(&self, image_ref: &str) -> Result<PathBuf, AgentError> {
        let reference: Reference = image_ref.parse().map_err(|e| AgentError::Runtime {
            detail: format!("invalid image reference '{image_ref}': {e}"),
        })?;

        let (manifest, digest, _config) = self
            .client
            .pull_manifest_and_config(&reference, &oci_client::secrets::RegistryAuth::Anonymous)
            .await
            .map_err(|e| AgentError::Runtime {
                detail: format!("failed to pull manifest for '{image_ref}': {e}"),
            })?;

        let safe_digest = digest.replace(':', "_");
        let image_dir = self.root_dir.join("images").join(&safe_digest);
        let rootfs_dir = image_dir.join("rootfs");
        let marker = image_dir.join(".complete");

        if marker.exists() {
            return Ok(rootfs_dir);
        }

        tokio::fs::create_dir_all(&rootfs_dir)
            .await
            .map_err(|e| AgentError::Runtime {
                detail: format!("failed to create rootfs directory: {e}"),
            })?;

        for layer in &manifest.layers {
            let mut stream = self
                .client
                .pull_blob_stream(&reference, layer)
                .await
                .map_err(|e| AgentError::Runtime {
                    detail: format!("failed to start pulling layer {}: {e}", layer.digest),
                })?;

            let mut layer_data = Vec::new();
            while let Some(result) = stream.next().await {
                let chunk = result.map_err(|e| AgentError::Runtime {
                    detail: format!("failed to read layer {} data: {e}", layer.digest),
                })?;
                layer_data.extend_from_slice(&chunk);
            }

            extract_layer(&layer_data, &rootfs_dir).await?;
        }

        tokio::fs::write(&marker, b"")
            .await
            .map_err(|e| AgentError::Runtime {
                detail: format!("failed to write image marker: {e}"),
            })?;

        Ok(rootfs_dir)
    }
}

async fn extract_layer(data: &[u8], rootfs: &PathBuf) -> Result<(), AgentError> {
    let rootfs = rootfs.clone();
    let data = data.to_vec();

    tokio::task::spawn_blocking(move || {
        let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(&data));
        let mut archive = tar::Archive::new(decoder);

        for entry in archive.entries().map_err(|e| AgentError::Runtime {
            detail: format!("failed to read tar entries: {e}"),
        })? {
            let mut entry = entry.map_err(|e| AgentError::Runtime {
                detail: format!("failed to read tar entry: {e}"),
            })?;

            let path = entry
                .path()
                .map_err(|e| AgentError::Runtime {
                    detail: format!("invalid tar entry path: {e}"),
                })?
                .to_path_buf();

            // Comp-5: reject obvious path-traversal shapes BEFORE join.
            // `entry.unpack(&dest)` with a pre-joined dest does not check
            // that the result stays within rootfs (unlike Archive::unpack).
            // A malicious OCI layer with `../../../etc/cron.d/evil` would
            // write host files under the agent's privilege.
            if path.is_absolute()
                || path.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir | std::path::Component::Prefix(_)
                    )
                })
            {
                return Err(AgentError::Runtime {
                    detail: format!(
                        "tar entry rejected (absolute or '..' component): {}",
                        path.display()
                    ),
                });
            }

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if let Some(whiteout_target) = name.strip_prefix(".wh.") {
                    let target = rootfs
                        .join(path.parent().unwrap_or(std::path::Path::new("")))
                        .join(whiteout_target);
                    // Defense-in-depth: belt-and-braces canonicalize check
                    // so a symlink inside the partially-extracted rootfs
                    // can't redirect the whiteout outside.
                    if let Ok(canonical) = target.canonicalize() {
                        if !canonical.starts_with(&rootfs) {
                            return Err(AgentError::Runtime {
                                detail: format!(
                                    "whiteout target escapes rootfs: {}",
                                    target.display()
                                ),
                            });
                        }
                    }
                    // OCI whiteout: target may not exist if a later layer already removed it
                    let _ = std::fs::remove_file(&target);
                    let _ = std::fs::remove_dir_all(&target);
                    continue;
                }
            }

            let dest = rootfs.join(&path);
            // Final symlink-following guard on the destination's parent.
            // A previous entry could have planted `etc -> /etc` and the
            // next entry exploits the link.
            if let Some(parent) = dest.parent() {
                if let Ok(canonical) = parent.canonicalize() {
                    if !canonical.starts_with(&rootfs) {
                        return Err(AgentError::Runtime {
                            detail: format!(
                                "tar entry parent escapes rootfs: {}",
                                dest.display()
                            ),
                        });
                    }
                }
            }
            entry.unpack(&dest).map_err(|e| AgentError::Runtime {
                detail: format!("failed to extract {}: {e}", path.display()),
            })?;
        }

        Ok(())
    })
    .await
    .map_err(|e| AgentError::Runtime {
        detail: format!("layer extraction task panicked: {e}"),
    })?
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
