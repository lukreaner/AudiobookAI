use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};

use crate::{MediaError, Result};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CacheFingerprint {
    pub schema_version: u32,
    pub text: String,
    pub context: Option<String>,
    pub provider_id: String,
    pub provider_endpoint_family: String,
    pub provider_version: Option<String>,
    pub model: Option<String>,
    pub voice: String,
    pub reference_audio_hashes: Vec<String>,
    pub settings: BTreeMap<String, serde_json::Value>,
    pub dictionary_revision: String,
    pub normalization_version: String,
}

impl CacheFingerprint {
    pub fn key(&self) -> Result<CacheKey> {
        let canonical = serde_json::to_vec(self)?;
        Ok(CacheKey(blake3::hash(&canonical).to_hex().to_string()))
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CacheKey(String);

impl CacheKey {
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            Ok(Self(value))
        } else {
            Err(MediaError::InvalidCacheKey)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct ContentAddressedCache {
    root: PathBuf,
}

impl ContentAddressedCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path(&self, key: &CacheKey) -> PathBuf {
        self.root
            .join("objects")
            .join(&key.as_str()[..2])
            .join(format!("{}.flac", key.as_str()))
    }

    pub fn manifest_path(&self, key: &CacheKey) -> PathBuf {
        self.path(key).with_extension("json")
    }

    pub fn contains(&self, key: &CacheKey) -> bool {
        self.path(key).is_file()
    }

    pub fn get(&self, key: &CacheKey) -> Result<Option<Vec<u8>>> {
        let path = self.path(key);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(MediaError::io(path, error)),
        }
    }

    /// Atomically stores a validated lossless artifact and its provenance manifest.
    pub fn put(
        &self,
        key: &CacheKey,
        flac: &[u8],
        manifest: &serde_json::Value,
    ) -> Result<PathBuf> {
        if flac.len() < 4 || &flac[..4] != b"fLaC" {
            return Err(MediaError::Configuration(
                "cache artifacts must be validated FLAC streams".to_owned(),
            ));
        }
        let manifest_bytes = serde_json::to_vec_pretty(manifest)?;
        let path = self.path(key);
        atomic_write(&path, flac)?;
        atomic_write(&self.manifest_path(key), &manifest_bytes)?;
        Ok(path)
    }

    pub fn pin(&self, key: &CacheKey) -> Result<()> {
        atomic_write(&self.path(key).with_extension("pin"), b"active\n")
    }

    pub fn unpin(&self, key: &CacheKey) -> Result<()> {
        let path = self.path(key).with_extension("pin");
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(MediaError::io(path, error)),
        }
    }

    /// Removes least-recently-modified unpinned objects until the cache fits `maximum_bytes`.
    pub fn prune(
        &self,
        maximum_bytes: u64,
        protected: &HashSet<CacheKey>,
    ) -> Result<CachePruneReport> {
        let objects = self.root.join("objects");
        if !objects.exists() {
            return Ok(CachePruneReport::default());
        }
        let mut candidates = Vec::new();
        collect_flac_files(&objects, &mut candidates)?;
        let mut total = candidates.iter().map(|item| item.size).sum::<u64>();
        let before = total;
        candidates.sort_by_key(|item| item.modified);
        let mut removed = 0;
        let mut removed_keys = Vec::new();
        for candidate in candidates {
            if total <= maximum_bytes {
                break;
            }
            if protected.contains(&candidate.key) || candidate.path.with_extension("pin").exists() {
                continue;
            }
            fs::remove_file(&candidate.path)
                .map_err(|source| MediaError::io(&candidate.path, source))?;
            remove_if_present(&candidate.path.with_extension("json"))?;
            total = total.saturating_sub(candidate.size);
            removed += 1;
            removed_keys.push(candidate.key);
        }
        Ok(CachePruneReport {
            bytes_before: before,
            bytes_after: total,
            objects_removed: removed,
            removed_keys,
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CachePruneReport {
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub objects_removed: u64,
    pub removed_keys: Vec<CacheKey>,
}

#[derive(Debug)]
struct Candidate {
    path: PathBuf,
    key: CacheKey,
    size: u64,
    modified: SystemTime,
}

fn collect_flac_files(directory: &Path, output: &mut Vec<Candidate>) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(|source| MediaError::io(directory, source))? {
        let entry = entry.map_err(|source| MediaError::io(directory, source))?;
        let path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|source| MediaError::io(&path, source))?;
        if metadata.is_dir() {
            collect_flac_files(&path, output)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "flac")
        {
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(key) = CacheKey::parse(stem) else {
                continue;
            };
            output.push(Candidate {
                path,
                key,
                size: metadata.len(),
                modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            });
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        MediaError::Configuration("cache destination has no parent directory".to_owned())
    })?;
    fs::create_dir_all(parent).map_err(|source| MediaError::io(parent, source))?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| MediaError::io(parent, source))?;
    temporary
        .write_all(contents)
        .map_err(|source| MediaError::io(temporary.path(), source))?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|source| MediaError::io(temporary.path(), source))?;
    temporary
        .persist(path)
        .map_err(|error| MediaError::io(path, error.error))?;
    sync_directory(parent)?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| MediaError::io(path, source))?;
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(MediaError::io(path, error)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::TempDir;

    use super::*;

    fn fingerprint(text: &str) -> CacheFingerprint {
        CacheFingerprint {
            schema_version: 1,
            text: text.to_owned(),
            context: None,
            provider_id: "localai".to_owned(),
            provider_endpoint_family: "openai-audio-v1".to_owned(),
            provider_version: Some("1".to_owned()),
            model: Some("kokoro".to_owned()),
            voice: "af_sky".to_owned(),
            reference_audio_hashes: Vec::new(),
            settings: BTreeMap::new(),
            dictionary_revision: "d1".to_owned(),
            normalization_version: "n1".to_owned(),
        }
    }

    #[test]
    fn fingerprint_is_deterministic_and_sensitive_to_text() {
        assert_eq!(
            fingerprint("one").key().unwrap(),
            fingerprint("one").key().unwrap()
        );
        assert_ne!(
            fingerprint("one").key().unwrap(),
            fingerprint("two").key().unwrap()
        );
    }

    #[test]
    fn atomically_stores_validated_flac_and_manifest() {
        let root = TempDir::new().unwrap();
        let cache = ContentAddressedCache::new(root.path());
        let key = fingerprint("one").key().unwrap();
        let path = cache
            .put(&key, b"fLaCpayload", &serde_json::json!({"ok": true}))
            .unwrap();
        assert_eq!(cache.get(&key).unwrap().unwrap(), b"fLaCpayload");
        assert!(path.is_file());
        assert!(cache.manifest_path(&key).is_file());
        assert!(cache.put(&key, b"wave", &serde_json::Value::Null).is_err());
    }

    #[test]
    fn prune_preserves_pinned_objects() {
        let root = TempDir::new().unwrap();
        let cache = ContentAddressedCache::new(root.path());
        let pinned = fingerprint("pinned").key().unwrap();
        let removable = fingerprint("removable").key().unwrap();
        cache
            .put(&pinned, b"fLaCpinned", &serde_json::Value::Null)
            .unwrap();
        cache
            .put(&removable, b"fLaCremove", &serde_json::Value::Null)
            .unwrap();
        cache.pin(&pinned).unwrap();
        let report = cache.prune(0, &HashSet::new()).unwrap();
        assert_eq!(report.objects_removed, 1);
        assert_eq!(report.removed_keys, vec![removable.clone()]);
        assert!(cache.contains(&pinned));
        assert!(!cache.contains(&removable));
    }
}
