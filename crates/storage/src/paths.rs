use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use directories::ProjectDirs;

use crate::{Result, StorageError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    pub root: PathBuf,
    pub managed_media_root: PathBuf,
    pub database: PathBuf,
    pub backups: PathBuf,
    pub library: PathBuf,
    pub cache: PathBuf,
    pub exports: PathBuf,
    pub logs: PathBuf,
    pub writer_lock: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let project_dirs =
            ProjectDirs::from("ai", "AudiobookAI", "AudiobookAI").ok_or_else(|| {
                StorageError::InvalidData(
                    "the operating system has no application data directory".into(),
                )
            })?;
        Ok(Self::from_root(project_dirs.data_dir()))
    }

    #[must_use]
    pub fn from_root(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self::from_roots(root, root)
    }

    /// Builds paths with private control data under `root` and large managed
    /// media under a separately selected root.
    #[must_use]
    pub fn from_roots(root: impl AsRef<Path>, managed_media_root: impl AsRef<Path>) -> Self {
        let root = root.as_ref().to_path_buf();
        let managed_media_root = managed_media_root.as_ref().to_path_buf();
        Self {
            database: root.join("audiobookai.sqlite3"),
            backups: root.join("backups"),
            library: managed_media_root.join("library"),
            cache: managed_media_root.join("cache"),
            exports: root.join("exports"),
            logs: root.join("logs"),
            writer_lock: root.join("audiobookai.writer.lock"),
            root,
            managed_media_root,
        }
    }

    pub async fn ensure(&self) -> Result<()> {
        for path in [&self.root, &self.backups, &self.exports, &self.logs] {
            tokio::fs::create_dir_all(path).await?;
            harden_private_directory(path).await?;
        }

        if self.managed_media_root == self.root {
            for path in [&self.library, &self.cache] {
                tokio::fs::create_dir_all(path).await?;
                harden_private_directory(path).await?;
            }
        } else {
            ensure_existing_real_directory(&self.managed_media_root).await?;
            for path in [&self.library, &self.cache] {
                tokio::fs::create_dir_all(path).await?;
                ensure_existing_real_directory(path).await?;
            }
        }
        Ok(())
    }
}

async fn ensure_existing_real_directory(path: &Path) -> Result<()> {
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StorageError::InvalidData(format!(
            "managed media path is not a non-symlinked directory: {}",
            path.display()
        )));
    }
    Ok(())
}

async fn harden_private_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;

    #[cfg(not(unix))]
    let _ = path;

    Ok(())
}

/// Restrict a managed data file to the current operating-system account.
pub async fn harden_private_file(path: impl AsRef<Path>) -> Result<()> {
    #[cfg(unix)]
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;

    #[cfg(not(unix))]
    let _ = path;

    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[tokio::test]
    async fn ensure_restricts_managed_directories_to_the_owner() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = temporary.path().join("data");
        let paths = AppPaths::from_root(&root);

        paths.ensure().await.expect("managed directories");

        for path in [
            &paths.root,
            &paths.backups,
            &paths.library,
            &paths.cache,
            &paths.exports,
            &paths.logs,
        ] {
            let mode = std::fs::metadata(path)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "unexpected permissions for {}", path.display());
        }
    }

    #[tokio::test]
    async fn private_file_permissions_are_owner_only() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("secret");
        tokio::fs::write(&path, b"not-a-real-secret")
            .await
            .expect("test file");

        harden_private_file(&path).await.expect("permissions");

        let mode = std::fs::metadata(&path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn external_media_keeps_control_data_local() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = temporary.path().join("data");
        let media_root = temporary.path().join("media");
        std::fs::create_dir(&media_root).expect("media root");
        let paths = AppPaths::from_roots(&root, &media_root);

        paths.ensure().await.expect("managed directories");

        assert_eq!(paths.database, root.join("audiobookai.sqlite3"));
        assert_eq!(paths.writer_lock, root.join("audiobookai.writer.lock"));
        assert_eq!(paths.library, media_root.join("library"));
        assert_eq!(paths.cache, media_root.join("cache"));
        assert!(paths.library.is_dir());
        assert!(paths.cache.is_dir());
        assert!(!media_root.join("audiobookai.sqlite3").exists());
    }
}
