use std::{
    fs::{self, File},
    io::Write as _,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use serde::{Deserialize, Serialize};

const CONFIG_VERSION: u8 = 2;
const LEGACY_CONFIG_VERSION: u8 = 1;
const MEDIA_ROOT_MARKER: &str = ".audiobookai-first-run-media-root";

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StorageLocationConfig {
    version: u8,
    data_root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    media_root: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfiguredStorageRoots {
    pub(crate) data_root: PathBuf,
    pub(crate) media_root: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum StagedMediaRoot {
    Unchanged,
    Ready { target: PathBuf, marker: String },
}

pub(crate) fn configured_storage_roots(
    config_path: &Path,
    default_root: &Path,
) -> Result<ConfiguredStorageRoots, String> {
    let metadata = match fs::symlink_metadata(config_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ConfiguredStorageRoots {
                data_root: default_root.to_path_buf(),
                media_root: default_root.to_path_buf(),
            });
        }
        Err(error) => {
            return Err(format!(
                "could not inspect the desktop storage configuration: {error}"
            ));
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(
            "the desktop storage configuration must be a regular, non-symlinked file".to_owned(),
        );
    }
    harden_private_file(config_path)?;
    let payload = fs::read(config_path)
        .map_err(|error| format!("could not read the desktop storage configuration: {error}"))?;
    let config: StorageLocationConfig = serde_json::from_slice(&payload)
        .map_err(|error| format!("the desktop storage configuration is invalid: {error}"))?;
    let data_root = configured_directory(&config.data_root, "desktop data path")?;
    let media_root = match config.version {
        LEGACY_CONFIG_VERSION => data_root.clone(),
        CONFIG_VERSION => configured_directory(
            config.media_root.as_deref().ok_or_else(|| {
                "the desktop storage configuration is missing its media root".to_owned()
            })?,
            "desktop media path",
        )?,
        version => {
            return Err(format!(
                "unsupported desktop storage configuration version {version}"
            ));
        }
    };
    Ok(ConfiguredStorageRoots {
        data_root,
        media_root,
    })
}

pub(crate) fn persist_storage_roots(
    config_path: &Path,
    data_root: &Path,
    media_root: &Path,
) -> Result<(), String> {
    let data_root = configured_directory(data_root, "desktop data path")?;
    let media_root = configured_directory(media_root, "desktop media path")?;
    let parent = config_path
        .parent()
        .ok_or_else(|| "the desktop storage configuration has no parent directory".to_owned())?;
    fs::create_dir_all(parent).map_err(|error| {
        format!("could not create the desktop configuration directory: {error}")
    })?;
    harden_private_directory(parent)?;

    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("could not create a temporary storage configuration: {error}"))?;
    serde_json::to_writer(
        temporary.as_file_mut(),
        &StorageLocationConfig {
            version: CONFIG_VERSION,
            data_root,
            media_root: Some(media_root),
        },
    )
    .map_err(|error| format!("could not encode the desktop storage configuration: {error}"))?;
    temporary
        .as_file_mut()
        .write_all(b"\n")
        .map_err(|error| format!("could not finish the desktop storage configuration: {error}"))?;
    harden_private_file(temporary.path())?;
    temporary
        .as_file_mut()
        .sync_all()
        .map_err(|error| format!("could not sync the desktop storage configuration: {error}"))?;
    temporary.persist(config_path).map_err(|error| {
        format!(
            "could not activate the desktop storage configuration: {}",
            error.error
        )
    })?;
    sync_directory(parent)?;
    Ok(())
}

pub(crate) fn stage_media_root(
    data_root: &Path,
    current_media_root: &Path,
    requested: &str,
) -> Result<StagedMediaRoot, String> {
    let Some(target) = media_root_target(data_root, current_media_root, requested)? else {
        return Ok(StagedMediaRoot::Unchanged);
    };

    let parent = target
        .parent()
        .ok_or_else(|| "the new media folder has no parent directory".to_owned())?;
    let staging = tempfile::Builder::new()
        .prefix(".audiobookai-media-root-")
        .tempdir_in(parent)
        .map_err(|error| format!("could not stage the new media folder: {error}"))?;
    let library = staging.path().join("library");
    let cache = staging.path().join("cache");
    fs::create_dir(&library)
        .and_then(|()| fs::create_dir(&cache))
        .map_err(|error| format!("could not create managed media folders: {error}"))?;
    verify_writable_directory(&library)?;
    verify_writable_directory(&cache)?;

    let marker = staging
        .path()
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "the media-root staging marker is invalid".to_owned())?
        .to_owned();
    let marker_path = staging.path().join(MEDIA_ROOT_MARKER);
    let mut marker_file = File::create(&marker_path)
        .map_err(|error| format!("could not create the media-root marker: {error}"))?;
    marker_file
        .write_all(marker.as_bytes())
        .and_then(|()| marker_file.sync_all())
        .map_err(|error| format!("could not sync the media-root marker: {error}"))?;
    drop(marker_file);
    sync_directory(staging.path())?;

    if target.exists() {
        fs::remove_dir(&target)
            .map_err(|error| format!("could not replace the empty media folder: {error}"))?;
    }
    let staging_path = staging.keep();
    if let Err(error) = fs::rename(&staging_path, &target) {
        let _ = fs::remove_dir_all(&staging_path);
        return Err(format!(
            "could not activate the managed media folder: {error}"
        ));
    }
    sync_directory(parent)?;
    Ok(StagedMediaRoot::Ready { target, marker })
}

/// Checks user-controlled media-root input while the service is still running.
///
/// Staging repeats these checks after shutdown so filesystem changes between the
/// preflight and activation cannot bypass the safety boundary.
pub(crate) fn validate_media_root_target(
    data_root: &Path,
    current_media_root: &Path,
    requested: &str,
) -> Result<bool, String> {
    Ok(media_root_target(data_root, current_media_root, requested)?.is_some())
}

fn media_root_target(
    data_root: &Path,
    current_media_root: &Path,
    requested: &str,
) -> Result<Option<PathBuf>, String> {
    let data_root = canonical_directory(data_root, "local desktop data path")?;
    let current_media_root = canonical_directory(current_media_root, "current media path")?;
    let requested = requested.trim();
    if requested.is_empty() {
        return Err("choose a media folder before continuing".to_owned());
    }
    let requested = PathBuf::from(requested);
    if !requested.is_absolute() {
        return Err("the media folder must be an absolute path".to_owned());
    }

    let target = prepare_target_path(&requested)?;
    if target == current_media_root {
        return Ok(None);
    }
    if target.starts_with(&data_root) || data_root.starts_with(&target) {
        return Err(
            "the media folder must be separate from the private local data folder".to_owned(),
        );
    }
    if target.starts_with(&current_media_root) || current_media_root.starts_with(&target) {
        return Err(
            "the new media folder must not contain, or be contained by, the current media folder"
                .to_owned(),
        );
    }
    verify_current_media_is_empty(&current_media_root)?;
    if target.exists() && directory_has_entries(&target)? {
        return Err("the new media folder must be empty".to_owned());
    }

    let parent = target
        .parent()
        .ok_or_else(|| "the new media folder has no parent directory".to_owned())?;
    let write_check = tempfile::Builder::new()
        .prefix(".audiobookai-write-check-")
        .tempdir_in(parent)
        .map_err(|error| format!("the new media folder is not writable: {error}"))?;
    write_check
        .close()
        .map_err(|error| format!("could not clean up the media write check: {error}"))?;
    Ok(Some(target))
}

fn verify_current_media_is_empty(media_root: &Path) -> Result<(), String> {
    for child in ["library", "cache"] {
        let path = media_root.join(child);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                if directory_has_entries(&path)? {
                    return Err(
                        "managed media cannot be moved after an import or cache entry exists"
                            .to_owned(),
                    );
                }
            }
            Ok(_) => {
                return Err(format!(
                    "the current managed {child} path is not a regular directory"
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "could not inspect the current managed {child} folder: {error}"
                ));
            }
        }
    }
    Ok(())
}

fn verify_writable_directory(path: &Path) -> Result<(), String> {
    let probe = path.join(".audiobookai-write-check");
    let expected = b"AudiobookAI managed-media write check";
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|error| format!("the managed media folder is not writable: {error}"))?;
    file.write_all(expected)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not sync the managed media write check: {error}"))?;
    drop(file);
    let actual = fs::read(&probe)
        .map_err(|error| format!("could not verify the managed media write check: {error}"))?;
    fs::remove_file(&probe)
        .map_err(|error| format!("could not clean up the managed media write check: {error}"))?;
    if actual != expected {
        return Err("the managed media folder did not preserve written data".to_owned());
    }
    sync_directory(path)
}

pub(crate) fn finish_media_root(target: &Path, marker: &str) -> Result<(), String> {
    verify_media_root_marker(target, marker)?;
    fs::remove_file(target.join(MEDIA_ROOT_MARKER))
        .map_err(|error| format!("could not remove the completed media-root marker: {error}"))?;
    sync_directory(target)
}

pub(crate) fn rollback_media_root(target: &Path, marker: &str) -> Result<(), String> {
    verify_media_root_marker(target, marker)?;
    fs::remove_dir_all(target)
        .map_err(|error| format!("could not roll back the staged media folder: {error}"))?;
    if let Some(parent) = target.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn prepare_target_path(requested: &Path) -> Result<PathBuf, String> {
    match fs::symlink_metadata(requested) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err("the new media path must be a non-symlinked directory".to_owned());
            }
            fs::canonicalize(requested)
                .map_err(|error| format!("could not resolve the new media folder: {error}"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let name = requested.file_name().ok_or_else(|| {
                "choose a dedicated folder instead of a filesystem root".to_owned()
            })?;
            let parent = requested
                .parent()
                .ok_or_else(|| "the new media folder has no parent directory".to_owned())?;
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create the media-folder parent: {error}"))?;
            let parent = canonical_directory(parent, "media-folder parent")?;
            Ok(parent.join(name))
        }
        Err(error) => Err(format!("could not inspect the new media folder: {error}")),
    }
}

fn configured_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("the configured {label} must be absolute"));
    }
    canonical_directory(path, &format!("configured {label}"))
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("could not inspect the {label}: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!("the {label} must be a non-symlinked directory"));
    }
    fs::canonicalize(path).map_err(|error| format!("could not resolve the {label}: {error}"))
}

fn directory_has_entries(path: &Path) -> Result<bool, String> {
    fs::read_dir(path)
        .map_err(|error| format!("could not inspect the media folder: {error}"))?
        .next()
        .transpose()
        .map(|entry| entry.is_some())
        .map_err(|error| format!("could not inspect the media folder: {error}"))
}

fn verify_media_root_marker(target: &Path, expected: &str) -> Result<(), String> {
    let target_metadata = fs::symlink_metadata(target)
        .map_err(|error| format!("could not inspect the managed media folder: {error}"))?;
    if !target_metadata.is_dir() || target_metadata.file_type().is_symlink() {
        return Err("the managed media folder changed unexpectedly".to_owned());
    }
    let marker_path = target.join(MEDIA_ROOT_MARKER);
    let marker_metadata = fs::symlink_metadata(&marker_path)
        .map_err(|error| format!("could not inspect the media-root marker: {error}"))?;
    if !marker_metadata.is_file() || marker_metadata.file_type().is_symlink() {
        return Err("the media-root marker changed unexpectedly".to_owned());
    }
    let actual = fs::read_to_string(marker_path)
        .map_err(|error| format!("could not read the media-root marker: {error}"))?;
    if actual != expected {
        return Err("the media-root marker no longer matches this operation".to_owned());
    }
    Ok(())
}

#[cfg(unix)]
fn harden_private_directory(path: &Path) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("could not protect the desktop configuration directory: {error}"))
}

#[cfg(not(unix))]
fn harden_private_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn harden_private_file(path: &Path) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("could not protect the desktop storage configuration: {error}"))
}

#[cfg(not(unix))]
fn harden_private_file(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("could not sync a storage directory: {error}"))?;

    #[cfg(not(unix))]
    let _ = path;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_roots_default_migrate_and_round_trip() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let default_root = temporary.path().join("default");
        let legacy_root = temporary.path().join("legacy");
        let media_root = temporary.path().join("media");
        let config_path = temporary
            .path()
            .join("config")
            .join("storage-location.json");
        fs::create_dir_all(&default_root).expect("default root");
        fs::create_dir_all(&legacy_root).expect("legacy root");
        fs::create_dir_all(&media_root).expect("media root");

        assert_eq!(
            configured_storage_roots(&config_path, &default_root).expect("default location"),
            ConfiguredStorageRoots {
                data_root: default_root.clone(),
                media_root: default_root.clone(),
            }
        );

        fs::create_dir_all(config_path.parent().expect("config parent")).expect("config parent");
        fs::write(
            &config_path,
            serde_json::json!({ "version": 1, "dataRoot": legacy_root })
                .to_string()
                .as_bytes(),
        )
        .expect("legacy config");
        let legacy = fs::canonicalize(&legacy_root).expect("canonical legacy root");
        assert_eq!(
            configured_storage_roots(&config_path, &default_root).expect("legacy location"),
            ConfiguredStorageRoots {
                data_root: legacy.clone(),
                media_root: legacy,
            }
        );

        persist_storage_roots(&config_path, &default_root, &media_root)
            .expect("persist selected media root");
        assert_eq!(
            configured_storage_roots(&config_path, &default_root).expect("configured location"),
            ConfiguredStorageRoots {
                data_root: fs::canonicalize(default_root).expect("canonical default root"),
                media_root: fs::canonicalize(media_root).expect("canonical media root"),
            }
        );
    }

    #[test]
    fn media_setup_creates_only_library_and_cache_outside_local_data() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let data_root = temporary.path().join("local-data");
        let target = temporary.path().join("selected-media");
        fs::create_dir_all(data_root.join("library")).expect("local library");
        fs::create_dir_all(data_root.join("cache")).expect("local cache");
        fs::write(data_root.join("audiobookai.sqlite3"), b"database").expect("database");
        fs::create_dir(&target).expect("empty selected folder");

        let StagedMediaRoot::Ready {
            target: configured,
            marker,
        } = stage_media_root(
            &data_root,
            &data_root,
            target.to_str().expect("UTF-8 target"),
        )
        .expect("staged media root")
        else {
            panic!("media setup unexpectedly reported no change");
        };
        assert_eq!(
            configured,
            fs::canonicalize(&target).expect("canonical target")
        );
        assert!(configured.join("library").is_dir());
        assert!(configured.join("cache").is_dir());
        assert!(!configured.join("audiobookai.sqlite3").exists());
        assert_eq!(
            fs::read(data_root.join("audiobookai.sqlite3")).expect("local database"),
            b"database"
        );
        finish_media_root(&configured, &marker).expect("finish media setup");
        assert!(!configured.join(MEDIA_ROOT_MARKER).exists());
    }

    #[test]
    fn media_setup_rejects_nonempty_overlapping_and_in_use_paths() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let data_root = temporary.path().join("local-data");
        let nonempty = temporary.path().join("nonempty");
        fs::create_dir_all(data_root.join("library")).expect("local library");
        fs::create_dir_all(data_root.join("cache")).expect("local cache");
        fs::create_dir_all(&nonempty).expect("target");
        fs::write(nonempty.join("unrelated.txt"), b"occupied").expect("occupied target");

        let nonempty_error = stage_media_root(
            &data_root,
            &data_root,
            nonempty.to_str().expect("UTF-8 target"),
        )
        .expect_err("nonempty target must fail");
        assert!(nonempty_error.contains("must be empty"));

        let nested = data_root.join("nested");
        let overlap_error = stage_media_root(
            &data_root,
            &data_root,
            nested.to_str().expect("UTF-8 target"),
        )
        .expect_err("nested target must fail");
        assert!(overlap_error.contains("separate"));

        fs::write(data_root.join("library").join("book.epub"), b"book").expect("managed book");
        let unused = temporary.path().join("unused");
        fs::create_dir(&unused).expect("empty target");
        let in_use_error = stage_media_root(
            &data_root,
            &data_root,
            unused.to_str().expect("UTF-8 target"),
        )
        .expect_err("in-use media root must fail");
        assert!(in_use_error.contains("cannot be moved"));
    }

    #[test]
    fn media_preflight_leaves_invalid_target_untouched() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let data_root = temporary.path().join("local-data");
        let empty = temporary.path().join("empty");
        let occupied = temporary.path().join("occupied");
        fs::create_dir_all(data_root.join("library")).expect("local library");
        fs::create_dir_all(data_root.join("cache")).expect("local cache");
        fs::create_dir_all(&empty).expect("empty target");
        fs::create_dir_all(&occupied).expect("occupied target");
        fs::write(occupied.join("existing.txt"), b"existing").expect("occupied file");

        assert!(
            !validate_media_root_target(
                &data_root,
                &data_root,
                data_root.to_str().expect("UTF-8 data root")
            )
            .expect("unchanged preflight")
        );
        assert!(
            validate_media_root_target(
                &data_root,
                &data_root,
                empty.to_str().expect("UTF-8 target")
            )
            .expect("valid preflight")
        );
        let error = validate_media_root_target(
            &data_root,
            &data_root,
            occupied.to_str().expect("UTF-8 target"),
        )
        .expect_err("occupied target must fail preflight");
        assert!(error.contains("must be empty"));
        assert_eq!(
            fs::read(occupied.join("existing.txt")).expect("existing file remains"),
            b"existing"
        );
    }

    #[test]
    fn media_rollback_only_removes_its_marked_root() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let data_root = temporary.path().join("local-data");
        let target = temporary.path().join("selected-media");
        fs::create_dir_all(data_root.join("library")).expect("local library");
        fs::create_dir_all(data_root.join("cache")).expect("local cache");

        let StagedMediaRoot::Ready {
            target: configured,
            marker,
        } = stage_media_root(
            &data_root,
            &data_root,
            target.to_str().expect("UTF-8 target"),
        )
        .expect("staged media root")
        else {
            panic!("media setup unexpectedly reported no change");
        };
        assert!(rollback_media_root(&configured, "wrong marker").is_err());
        assert!(configured.exists());
        rollback_media_root(&configured, &marker).expect("rollback media setup");
        assert!(!configured.exists());
        assert!(data_root.exists());
    }
}
