use std::{
    env,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{MediaError, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SidecarPair {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SidecarChecksums {
    /// Lowercase hexadecimal SHA-256 for the matching executable.
    pub ffmpeg_sha256: String,
    pub ffprobe_sha256: String,
}

/// Resolves packaged sidecars first and optionally permits explicitly configured/system binaries.
#[derive(Clone, Debug)]
pub struct SidecarResolver {
    bundled_directory: PathBuf,
    explicit: Option<SidecarPair>,
    allow_system_path: bool,
    checksums: Option<SidecarChecksums>,
}

impl SidecarResolver {
    pub fn bundled(bundled_directory: impl Into<PathBuf>) -> Self {
        Self {
            bundled_directory: bundled_directory.into(),
            explicit: None,
            allow_system_path: false,
            checksums: None,
        }
    }

    #[must_use]
    pub fn explicit(mut self, ffmpeg: impl Into<PathBuf>, ffprobe: impl Into<PathBuf>) -> Self {
        self.explicit = Some(SidecarPair {
            ffmpeg: ffmpeg.into(),
            ffprobe: ffprobe.into(),
        });
        self
    }

    #[must_use]
    pub fn allow_system_path(mut self, allow: bool) -> Self {
        self.allow_system_path = allow;
        self
    }

    #[must_use]
    pub fn checksums(mut self, checksums: SidecarChecksums) -> Self {
        self.checksums = Some(checksums);
        self
    }

    pub fn resolve(&self) -> Result<SidecarPair> {
        let mut checked = Vec::new();
        if let Some(pair) = &self.explicit {
            checked.extend([pair.ffmpeg.clone(), pair.ffprobe.clone()]);
            if pair_is_executable(pair) {
                self.verify(pair)?;
                return canonical_pair(pair);
            }
        }

        let pair = SidecarPair {
            ffmpeg: self.bundled_directory.join(executable_name("ffmpeg")),
            ffprobe: self.bundled_directory.join(executable_name("ffprobe")),
        };
        checked.extend([pair.ffmpeg.clone(), pair.ffprobe.clone()]);
        if pair_is_executable(&pair) {
            let canonical_root = self
                .bundled_directory
                .canonicalize()
                .map_err(|source| MediaError::io(&self.bundled_directory, source))?;
            let pair = canonical_pair(&pair)?;
            if !pair.ffmpeg.starts_with(&canonical_root)
                || !pair.ffprobe.starts_with(&canonical_root)
            {
                return Err(MediaError::Configuration(
                    "bundled sidecars may not escape their application directory".to_owned(),
                ));
            }
            self.verify(&pair)?;
            return Ok(pair);
        }

        if self.allow_system_path
            && let (Some(ffmpeg), Some(ffprobe)) = (
                find_on_path(&executable_name("ffmpeg"), &mut checked),
                find_on_path(&executable_name("ffprobe"), &mut checked),
            )
        {
            let pair = SidecarPair { ffmpeg, ffprobe };
            self.verify(&pair)?;
            return canonical_pair(&pair);
        }
        Err(MediaError::SidecarNotFound { checked })
    }

    fn verify(&self, pair: &SidecarPair) -> Result<()> {
        let Some(checksums) = &self.checksums else {
            return Ok(());
        };
        verify_sha256(&pair.ffmpeg, &checksums.ffmpeg_sha256)?;
        verify_sha256(&pair.ffprobe, &checksums.ffprobe_sha256)
    }
}

fn executable_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_owned()
    }
}

fn pair_is_executable(pair: &SidecarPair) -> bool {
    is_executable(&pair.ffmpeg) && is_executable(&pair.ffprobe)
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn canonical_pair(pair: &SidecarPair) -> Result<SidecarPair> {
    Ok(SidecarPair {
        ffmpeg: pair
            .ffmpeg
            .canonicalize()
            .map_err(|source| MediaError::io(&pair.ffmpeg, source))?,
        ffprobe: pair
            .ffprobe
            .canonicalize()
            .map_err(|source| MediaError::io(&pair.ffprobe, source))?,
    })
}

fn find_on_path(name: &str, checked: &mut Vec<PathBuf>) -> Option<PathBuf> {
    for directory in env::var_os("PATH")
        .as_deref()
        .map(env::split_paths)
        .into_iter()
        .flatten()
    {
        let candidate = directory.join(name);
        checked.push(candidate.clone());
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(MediaError::Configuration(
            "sidecar SHA-256 must be 64 hexadecimal characters".to_owned(),
        ));
    }
    let mut file = File::open(path).map_err(|source| MediaError::io(path, source))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| MediaError::io(path, source))?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hash.finalize());
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(MediaError::ChecksumMismatch {
            path: path.to_path_buf(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[cfg(unix)]
    fn executable(path: &Path, contents: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, contents).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn resolves_preserved_packaged_bin_directory_without_running_it() {
        let resources = TempDir::new().unwrap();
        let directory = resources.path().join("sidecars").join("bin");
        fs::create_dir_all(&directory).unwrap();
        executable(&directory.join("ffmpeg"), b"ffmpeg");
        executable(&directory.join("ffprobe"), b"ffprobe");
        let pair = SidecarResolver::bundled(&directory).resolve().unwrap();
        assert!(pair.ffmpeg.is_absolute());
        assert!(pair.ffprobe.is_absolute());
        let canonical_directory = directory.canonicalize().unwrap();
        assert_eq!(pair.ffmpeg.parent(), Some(canonical_directory.as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn verifies_pinned_checksum() {
        let directory = TempDir::new().unwrap();
        executable(&directory.path().join("ffmpeg"), b"ffmpeg");
        executable(&directory.path().join("ffprobe"), b"ffprobe");
        let checksums = SidecarChecksums {
            ffmpeg_sha256: format!("{:x}", Sha256::digest(b"ffmpeg")),
            ffprobe_sha256: format!("{:x}", Sha256::digest(b"ffprobe")),
        };
        assert!(
            SidecarResolver::bundled(directory.path())
                .checksums(checksums)
                .resolve()
                .is_ok()
        );
    }
}
