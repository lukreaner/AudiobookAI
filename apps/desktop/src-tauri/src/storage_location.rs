use std::{
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use serde::{Deserialize, Serialize};

const CONFIG_VERSION: u8 = 1;
const RELOCATION_MARKER: &str = ".audiobookai-first-run-relocation";

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StorageLocationConfig {
    version: u8,
    data_root: PathBuf,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum StagedRelocation {
    Unchanged,
    Ready { target: PathBuf, marker: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManifestEntry {
    relative_path: PathBuf,
    kind: ManifestEntryKind,
    size: u64,
    permissions: u32,
    digest: Option<blake3::Hash>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManifestEntryKind {
    Directory,
    File,
}

pub(crate) fn configured_data_root(
    config_path: &Path,
    default_root: &Path,
) -> Result<PathBuf, String> {
    let metadata = match fs::symlink_metadata(config_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(default_root.to_path_buf());
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
    if config.version != CONFIG_VERSION {
        return Err(format!(
            "unsupported desktop storage configuration version {}",
            config.version
        ));
    }
    if !config.data_root.is_absolute() {
        return Err("the configured desktop storage path must be absolute".to_owned());
    }
    let metadata = fs::symlink_metadata(&config.data_root)
        .map_err(|error| format!("the configured desktop storage path is unavailable: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(
            "the configured desktop storage path must be a non-symlinked directory".to_owned(),
        );
    }
    fs::canonicalize(&config.data_root)
        .map_err(|error| format!("could not resolve the configured desktop storage path: {error}"))
}

pub(crate) fn persist_data_root(config_path: &Path, data_root: &Path) -> Result<(), String> {
    if !data_root.is_absolute() {
        return Err("the desktop storage path must be absolute".to_owned());
    }
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
            data_root: data_root.to_path_buf(),
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

pub(crate) fn stage_relocation(source: &Path, requested: &str) -> Result<StagedRelocation, String> {
    let Some((source, target)) = relocation_paths(source, requested)? else {
        return Ok(StagedRelocation::Unchanged);
    };

    let parent = target
        .parent()
        .ok_or_else(|| "the new storage folder has no parent directory".to_owned())?;
    let staging = tempfile::Builder::new()
        .prefix(".audiobookai-relocation-")
        .tempdir_in(parent)
        .map_err(|error| format!("could not stage the new storage folder: {error}"))?;
    copy_directory_contents(&source, staging.path())?;
    fs::set_permissions(
        staging.path(),
        fs::metadata(&source).map_err(io_error)?.permissions(),
    )
    .map_err(|error| format!("could not preserve storage-folder permissions: {error}"))?;

    let source_manifest = build_manifest(&source)?;
    let target_manifest = build_manifest(staging.path())?;
    if source_manifest != target_manifest {
        return Err("the copied storage data did not pass verification".to_owned());
    }

    let marker = staging
        .path()
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "the relocation staging marker is invalid".to_owned())?
        .to_owned();
    let marker_path = staging.path().join(RELOCATION_MARKER);
    let mut marker_file = File::create(&marker_path)
        .map_err(|error| format!("could not create the relocation marker: {error}"))?;
    marker_file
        .write_all(marker.as_bytes())
        .and_then(|()| marker_file.sync_all())
        .map_err(|error| format!("could not sync the relocation marker: {error}"))?;
    harden_private_file(&marker_path)?;
    drop(marker_file);

    if target.exists() {
        fs::remove_dir(&target)
            .map_err(|error| format!("could not replace the empty storage folder: {error}"))?;
    }
    let staging_path = staging.keep();
    if let Err(error) = fs::rename(&staging_path, &target) {
        let _ = fs::remove_dir_all(&staging_path);
        return Err(format!(
            "could not activate the copied storage folder: {error}"
        ));
    }
    sync_directory(parent)?;
    Ok(StagedRelocation::Ready { target, marker })
}

/// Checks user-controlled relocation input while the service is still running.
///
/// Staging repeats these checks after shutdown so filesystem changes between the
/// preflight and the copy cannot bypass the safety boundary.
pub(crate) fn validate_relocation_target(source: &Path, requested: &str) -> Result<bool, String> {
    Ok(relocation_paths(source, requested)?.is_some())
}

fn relocation_paths(source: &Path, requested: &str) -> Result<Option<(PathBuf, PathBuf)>, String> {
    let source = canonical_directory(source, "current desktop storage path")?;
    let requested = requested.trim();
    if requested.is_empty() {
        return Err("choose a storage folder before continuing".to_owned());
    }
    let requested = PathBuf::from(requested);
    if !requested.is_absolute() {
        return Err("the storage folder must be an absolute path".to_owned());
    }

    let target = prepare_target_path(&requested)?;
    if target == source {
        return Ok(None);
    }
    if target.starts_with(&source) || source.starts_with(&target) {
        return Err(
            "the new storage folder must not contain, or be contained by, the current storage folder"
                .to_owned(),
        );
    }
    if target.exists() && directory_has_entries(&target)? {
        return Err("the new storage folder must be empty".to_owned());
    }

    let parent = target
        .parent()
        .ok_or_else(|| "the new storage folder has no parent directory".to_owned())?;
    let write_check = tempfile::Builder::new()
        .prefix(".audiobookai-write-check-")
        .tempdir_in(parent)
        .map_err(|error| format!("the new storage folder is not writable: {error}"))?;
    write_check
        .close()
        .map_err(|error| format!("could not clean up the storage write check: {error}"))?;
    Ok(Some((source, target)))
}

pub(crate) fn finish_relocation(target: &Path, marker: &str) -> Result<(), String> {
    verify_relocation_marker(target, marker)?;
    fs::remove_file(target.join(RELOCATION_MARKER))
        .map_err(|error| format!("could not remove the completed relocation marker: {error}"))?;
    sync_directory(target)
}

pub(crate) fn rollback_relocation(target: &Path, marker: &str) -> Result<(), String> {
    verify_relocation_marker(target, marker)?;
    fs::remove_dir_all(target)
        .map_err(|error| format!("could not roll back the staged storage folder: {error}"))?;
    if let Some(parent) = target.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn prepare_target_path(requested: &Path) -> Result<PathBuf, String> {
    match fs::symlink_metadata(requested) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err("the new storage path must be a non-symlinked directory".to_owned());
            }
            fs::canonicalize(requested)
                .map_err(|error| format!("could not resolve the new storage folder: {error}"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let name = requested.file_name().ok_or_else(|| {
                "choose a dedicated folder instead of a filesystem root".to_owned()
            })?;
            let parent = requested
                .parent()
                .ok_or_else(|| "the new storage folder has no parent directory".to_owned())?;
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create the storage-folder parent: {error}"))?;
            let parent = canonical_directory(parent, "storage-folder parent")?;
            Ok(parent.join(name))
        }
        Err(error) => Err(format!("could not inspect the new storage folder: {error}")),
    }
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
        .map_err(|error| format!("could not inspect the new storage folder: {error}"))?
        .next()
        .transpose()
        .map(|entry| entry.is_some())
        .map_err(|error| format!("could not inspect the new storage folder: {error}"))
}

fn copy_directory_contents(source: &Path, target: &Path) -> Result<(), String> {
    for entry in fs::read_dir(source)
        .map_err(|error| format!("could not read the current storage folder: {error}"))?
    {
        let entry = entry.map_err(io_error)?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path).map_err(io_error)?;
        if metadata.file_type().is_symlink() {
            return Err("managed storage must not contain symbolic links during setup".to_owned());
        }
        if metadata.is_dir() {
            fs::create_dir(&target_path).map_err(io_error)?;
            copy_directory_contents(&source_path, &target_path)?;
            fs::set_permissions(&target_path, metadata.permissions()).map_err(io_error)?;
        } else if metadata.is_file() {
            let copied = fs::copy(&source_path, &target_path).map_err(io_error)?;
            if copied != metadata.len() {
                return Err("a managed storage file was not copied completely".to_owned());
            }
            OpenOptions::new()
                .write(true)
                .open(&target_path)
                .and_then(|file| file.sync_all())
                .map_err(io_error)?;
            fs::set_permissions(&target_path, metadata.permissions()).map_err(io_error)?;
        } else {
            return Err("managed storage contains an unsupported special file".to_owned());
        }
    }
    sync_directory(target)
}

fn build_manifest(root: &Path) -> Result<Vec<ManifestEntry>, String> {
    let mut entries = Vec::new();
    collect_manifest(root, root, &mut entries)?;
    entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(entries)
}

fn collect_manifest(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<ManifestEntry>,
) -> Result<(), String> {
    for entry in fs::read_dir(directory).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
        let relative_path = path
            .strip_prefix(root)
            .map_err(|_| "could not build a relative storage manifest".to_owned())?
            .to_path_buf();
        if metadata.file_type().is_symlink() {
            return Err("managed storage must not contain symbolic links during setup".to_owned());
        }
        if metadata.is_dir() {
            entries.push(ManifestEntry {
                relative_path,
                kind: ManifestEntryKind::Directory,
                size: 0,
                permissions: permission_fingerprint(&metadata),
                digest: None,
            });
            collect_manifest(root, &path, entries)?;
        } else if metadata.is_file() {
            entries.push(ManifestEntry {
                relative_path,
                kind: ManifestEntryKind::File,
                size: metadata.len(),
                permissions: permission_fingerprint(&metadata),
                digest: Some(hash_file(&path)?),
            });
        } else {
            return Err("managed storage contains an unsupported special file".to_owned());
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<blake3::Hash, String> {
    let mut file = File::open(path).map_err(io_error)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize())
}

fn verify_relocation_marker(target: &Path, expected: &str) -> Result<(), String> {
    let target_metadata = fs::symlink_metadata(target)
        .map_err(|error| format!("could not inspect the relocated storage folder: {error}"))?;
    if !target_metadata.is_dir() || target_metadata.file_type().is_symlink() {
        return Err("the relocated storage folder changed unexpectedly".to_owned());
    }
    let marker_path = target.join(RELOCATION_MARKER);
    let marker_metadata = fs::symlink_metadata(&marker_path)
        .map_err(|error| format!("could not inspect the relocation marker: {error}"))?;
    if !marker_metadata.is_file() || marker_metadata.file_type().is_symlink() {
        return Err("the relocation marker changed unexpectedly".to_owned());
    }
    let actual = fs::read_to_string(marker_path)
        .map_err(|error| format!("could not read the relocation marker: {error}"))?;
    if actual != expected {
        return Err("the relocation marker no longer matches this operation".to_owned());
    }
    Ok(())
}

#[cfg(unix)]
fn permission_fingerprint(metadata: &fs::Metadata) -> u32 {
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn permission_fingerprint(metadata: &fs::Metadata) -> u32 {
    u32::from(metadata.permissions().readonly())
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

#[allow(clippy::needless_pass_by_value)] // Adapter for Result::map_err, which passes errors by value.
fn io_error(error: std::io::Error) -> String {
    format!("storage relocation failed: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_root_defaults_and_round_trips() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let default_root = temporary.path().join("default");
        let selected_root = temporary.path().join("selected");
        let config_path = temporary
            .path()
            .join("config")
            .join("storage-location.json");
        fs::create_dir_all(&default_root).expect("default root");
        fs::create_dir_all(&selected_root).expect("selected root");

        assert_eq!(
            configured_data_root(&config_path, &default_root).expect("default location"),
            default_root
        );
        persist_data_root(&config_path, &selected_root).expect("persist selected root");
        assert_eq!(
            configured_data_root(&config_path, &default_root).expect("configured location"),
            fs::canonicalize(selected_root).expect("canonical selected root")
        );
    }

    #[test]
    fn relocation_copies_and_verifies_the_managed_tree() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let source = temporary.path().join("source");
        let target = temporary.path().join("selected");
        fs::create_dir_all(source.join("library")).expect("library");
        fs::create_dir_all(source.join("cache")).expect("cache");
        fs::create_dir(&target).expect("empty selected folder");
        fs::write(source.join("audiobookai.sqlite3"), b"database").expect("database");
        fs::write(source.join("library").join("book.epub"), b"book").expect("book");

        let StagedRelocation::Ready {
            target: relocated,
            marker,
        } = stage_relocation(&source, target.to_str().expect("UTF-8 target"))
            .expect("staged relocation")
        else {
            panic!("relocation unexpectedly reported no change");
        };
        assert_eq!(
            relocated,
            fs::canonicalize(&target).expect("canonical target")
        );
        assert_eq!(
            fs::read(relocated.join("library").join("book.epub")).expect("copied book"),
            b"book"
        );
        finish_relocation(&relocated, &marker).expect("finish relocation");
        assert!(!relocated.join(RELOCATION_MARKER).exists());
    }

    #[test]
    fn relocation_rejects_nonempty_and_overlapping_targets() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let source = temporary.path().join("source");
        let nonempty = temporary.path().join("nonempty");
        fs::create_dir_all(source.join("library")).expect("source");
        fs::create_dir_all(&nonempty).expect("target");
        fs::write(nonempty.join("unrelated.txt"), b"occupied").expect("occupied target");

        let nonempty_error = stage_relocation(&source, nonempty.to_str().expect("UTF-8 target"))
            .expect_err("nonempty target must fail");
        assert!(nonempty_error.contains("must be empty"));

        let nested = source.join("nested");
        let overlap_error = stage_relocation(&source, nested.to_str().expect("UTF-8 target"))
            .expect_err("nested target must fail");
        assert!(overlap_error.contains("must not contain"));
    }

    #[test]
    fn relocation_preflight_does_not_mutate_or_stop_for_invalid_input() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let source = temporary.path().join("source");
        let empty = temporary.path().join("empty");
        let occupied = temporary.path().join("occupied");
        fs::create_dir_all(&source).expect("source");
        fs::create_dir_all(&empty).expect("empty target");
        fs::create_dir_all(&occupied).expect("occupied target");
        fs::write(occupied.join("existing.txt"), b"existing").expect("occupied file");

        assert!(
            !validate_relocation_target(&source, source.to_str().expect("UTF-8 source"))
                .expect("unchanged preflight")
        );
        assert!(
            validate_relocation_target(&source, empty.to_str().expect("UTF-8 target"))
                .expect("valid preflight")
        );
        let error = validate_relocation_target(&source, occupied.to_str().expect("UTF-8 target"))
            .expect_err("occupied target must fail preflight");
        assert!(error.contains("must be empty"));
        assert_eq!(
            fs::read(occupied.join("existing.txt")).expect("existing file remains"),
            b"existing"
        );
    }

    #[test]
    fn relocation_rollback_only_removes_its_marked_copy() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let source = temporary.path().join("source");
        let target = temporary.path().join("selected");
        fs::create_dir_all(&source).expect("source");
        fs::write(source.join("audiobookai.sqlite3"), b"database").expect("database");

        let StagedRelocation::Ready {
            target: relocated,
            marker,
        } = stage_relocation(&source, target.to_str().expect("UTF-8 target"))
            .expect("staged relocation")
        else {
            panic!("relocation unexpectedly reported no change");
        };
        assert!(rollback_relocation(&relocated, "wrong marker").is_err());
        assert!(relocated.exists());
        rollback_relocation(&relocated, &marker).expect("rollback relocation");
        assert!(!relocated.exists());
        assert!(source.exists());
    }
}
