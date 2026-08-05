//! Hardened EPUB 2/3 inspection and semantic text extraction.

use std::{
    fs::File,
    io::{Read, Seek},
    path::{Component, Path, PathBuf},
};

use rbook::Epub;
use scraper::{ElementRef, Html};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zip::ZipArchive;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportLimits {
    pub max_archive_bytes: u64,
    pub max_entries: usize,
    pub max_entry_bytes: u64,
    pub max_expanded_bytes: u64,
    pub max_compression_ratio: u64,
    pub max_content_document_bytes: u64,
    pub max_cover_bytes: u64,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_archive_bytes: 1_073_741_824,
            max_entries: 10_000,
            max_entry_bytes: 134_217_728,
            max_expanded_bytes: 2_147_483_648,
            max_compression_ratio: 200,
            max_content_document_bytes: 16_777_216,
            max_cover_bytes: 33_554_432,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ArchiveInspection {
    pub entry_count: usize,
    pub compressed_bytes: u64,
    pub expanded_bytes: u64,
    pub has_encryption_manifest: bool,
    pub drm_protected: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ImportedMetadata {
    pub title: String,
    pub authors: Vec<String>,
    pub language: Option<String>,
    pub identifier: Option<String>,
    pub description: Option<String>,
    pub series: Option<String>,
    pub series_position: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportedParagraph {
    pub source_index: usize,
    pub kind: ParagraphKind,
    pub text: String,
    pub start_offset: usize,
    pub end_offset: usize,
    pub content_hash: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParagraphKind {
    Heading,
    Paragraph,
    ListItem,
    Quote,
    Preformatted,
    ImageDescription,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportedChapter {
    pub order: usize,
    pub source_href: String,
    pub title: String,
    pub linear: bool,
    pub text: String,
    pub sanitized_html: String,
    pub paragraphs: Vec<ImportedParagraph>,
    pub content_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportedCover {
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub content_hash: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportedEpub {
    pub source: PathBuf,
    pub inspection: ArchiveInspection,
    pub metadata: ImportedMetadata,
    pub chapters: Vec<ImportedChapter>,
    pub cover: Option<ImportedCover>,
}

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("EPUB file could not be opened: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid EPUB ZIP: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("EPUB is larger than the configured {limit} byte limit")]
    ArchiveTooLarge { limit: u64 },
    #[error("EPUB contains too many files ({actual}; limit {limit})")]
    TooManyEntries { actual: usize, limit: usize },
    #[error("EPUB entry is unsafe: {0}")]
    UnsafeEntry(String),
    #[error("EPUB expanded content exceeds the configured limit")]
    ExpandedContentTooLarge,
    #[error("EPUB entry has a suspicious compression ratio: {0}")]
    SuspiciousCompression(String),
    #[error("EPUB contains encrypted ZIP entries")]
    ZipEncryption,
    #[error("EPUB is DRM protected; provide a lawful DRM-free copy")]
    DrmProtected,
    #[error("EPUB parser rejected the publication: {0}")]
    Parse(String),
    #[error("EPUB contains no readable chapters")]
    NoReadableChapters,
    #[error("EPUB content document exceeds the configured limit")]
    ContentDocumentTooLarge,
    #[error("EPUB cover exceeds the configured limit")]
    CoverTooLarge,
}

/// Inspects an EPUB archive without importing its publication content.
///
/// # Errors
///
/// Returns [`ImportError`] when the file cannot be read as a ZIP archive or an
/// archive entry violates the configured safety limits.
pub fn inspect(path: &Path, limits: &ImportLimits) -> Result<ArchiveInspection, ImportError> {
    let archive_bytes = std::fs::metadata(path)?.len();
    if archive_bytes > limits.max_archive_bytes {
        return Err(ImportError::ArchiveTooLarge {
            limit: limits.max_archive_bytes,
        });
    }

    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    if archive.len() > limits.max_entries {
        return Err(ImportError::TooManyEntries {
            actual: archive.len(),
            limit: limits.max_entries,
        });
    }

    let mut expanded_bytes = 0_u64;
    let mut compressed_bytes = 0_u64;
    let mut has_encryption_manifest = false;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        validate_entry_path(entry.name(), entry.enclosed_name().as_deref())?;
        if entry.encrypted() {
            return Err(ImportError::ZipEncryption);
        }
        if entry.size() > limits.max_entry_bytes {
            return Err(ImportError::UnsafeEntry(format!(
                "{} exceeds the per-file limit",
                entry.name()
            )));
        }
        expanded_bytes = expanded_bytes
            .checked_add(entry.size())
            .ok_or(ImportError::ExpandedContentTooLarge)?;
        compressed_bytes = compressed_bytes.saturating_add(entry.compressed_size());
        if expanded_bytes > limits.max_expanded_bytes {
            return Err(ImportError::ExpandedContentTooLarge);
        }
        if entry.compressed_size() > 0
            && entry.size() / entry.compressed_size().max(1) > limits.max_compression_ratio
        {
            return Err(ImportError::SuspiciousCompression(entry.name().to_owned()));
        }
        has_encryption_manifest |= entry.name().eq_ignore_ascii_case("META-INF/encryption.xml");
    }

    let drm_protected = if has_encryption_manifest {
        encryption_manifest_has_drm(&mut archive)?
    } else {
        false
    };

    Ok(ArchiveInspection {
        entry_count: archive.len(),
        compressed_bytes,
        expanded_bytes,
        has_encryption_manifest,
        drm_protected,
    })
}

/// Imports the metadata, cover, and ordered readable chapters from an EPUB.
///
/// # Errors
///
/// Returns [`ImportError`] when archive inspection fails, DRM is detected, the
/// publication cannot be parsed, or it contains no readable chapters.
pub fn import(path: &Path, limits: &ImportLimits) -> Result<ImportedEpub, ImportError> {
    let inspection = inspect(path, limits)?;
    if inspection.drm_protected {
        return Err(ImportError::DrmProtected);
    }

    let epub = Epub::open(path).map_err(|error| ImportError::Parse(error.to_string()))?;
    let metadata_view = epub.metadata();
    let metadata = ImportedMetadata {
        title: metadata_view
            .title()
            .map_or_else(|| "Untitled".to_owned(), |entry| entry.value().to_owned()),
        authors: metadata_view
            .creators()
            .map(|entry| entry.value().to_owned())
            .collect(),
        language: metadata_view
            .language()
            .map(|entry| entry.value().to_owned()),
        identifier: metadata_view
            .identifier()
            .map(|entry| entry.value().to_owned()),
        description: metadata_view
            .description()
            .map(|entry| entry.value().to_owned()),
        series: metadata_view
            .by_property("belongs-to-collection")
            .find(|entry| {
                entry
                    .refinements()
                    .by_property("collection-type")
                    .any(|refinement| refinement.value() == "series")
            })
            .map(|entry| entry.value().to_owned())
            .or_else(|| {
                metadata_view
                    .by_property("calibre:series")
                    .next()
                    .map(|entry| entry.value().to_owned())
            }),
        series_position: metadata_view
            .by_property("belongs-to-collection")
            .find_map(|entry| {
                entry
                    .refinements()
                    .by_property("group-position")
                    .next()
                    .map(|refinement| refinement.value().to_owned())
            })
            .or_else(|| {
                metadata_view
                    .by_property("calibre:series_index")
                    .next()
                    .map(|entry| entry.value().to_owned())
            }),
    };

    let mut chapters = Vec::new();
    for content in epub.reader() {
        let content = content.map_err(|error| ImportError::Parse(error.to_string()))?;
        if content.content().len() as u64 > limits.max_content_document_bytes {
            return Err(ImportError::ContentDocumentTooLarge);
        }
        let spine_entry = content.spine_entry();
        let manifest_entry = content.manifest_entry();
        let source_href = manifest_entry.href().as_str().to_owned();
        let chapter = extract_chapter(
            content.position(),
            source_href,
            spine_entry.is_linear(),
            content.content(),
        );
        if !chapter.paragraphs.is_empty() {
            chapters.push(chapter);
        }
    }
    if chapters.is_empty() {
        return Err(ImportError::NoReadableChapters);
    }

    let cover = epub
        .manifest()
        .cover_image()
        .map(|entry| {
            let bytes = entry
                .read_bytes()
                .map_err(|error| ImportError::Parse(error.to_string()))?;
            if bytes.len() as u64 > limits.max_cover_bytes {
                return Err(ImportError::CoverTooLarge);
            }
            Ok(ImportedCover {
                media_type: entry.media_type().to_owned(),
                content_hash: blake3::hash(&bytes).to_hex().to_string(),
                bytes,
            })
        })
        .transpose()?;

    Ok(ImportedEpub {
        source: path.to_path_buf(),
        inspection,
        metadata,
        chapters,
        cover,
    })
}

fn validate_entry_path(name: &str, enclosed: Option<&Path>) -> Result<(), ImportError> {
    let Some(path) = enclosed else {
        return Err(ImportError::UnsafeEntry(name.to_owned()));
    };
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ImportError::UnsafeEntry(name.to_owned()));
    }
    Ok(())
}

fn encryption_manifest_has_drm<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<bool, ImportError> {
    let mut entry = archive.by_name("META-INF/encryption.xml")?;
    if entry.size() > 1_048_576 {
        return Err(ImportError::UnsafeEntry(
            "META-INF/encryption.xml is too large".to_owned(),
        ));
    }
    let mut xml = String::new();
    entry.read_to_string(&mut xml)?;
    let allowed_font_algorithms = [
        "http://www.idpf.org/2008/embedding",
        "http://ns.adobe.com/pdf/enc#RC",
    ];
    let algorithms = encryption_algorithms(&xml);
    Ok(algorithms
        .iter()
        .any(|algorithm| !allowed_font_algorithms.contains(&algorithm.as_str())))
}

fn encryption_algorithms(xml: &str) -> Vec<String> {
    let mut values = Vec::new();
    for marker in ["Algorithm=\"", "Algorithm='"] {
        let quote = marker.chars().last().expect("marker quote");
        let mut remainder = xml;
        while let Some(start) = remainder.find(marker) {
            let value_start = start + marker.len();
            let after = &remainder[value_start..];
            if let Some(end) = after.find(quote) {
                values.push(after[..end].to_owned());
                remainder = &after[end + 1..];
            } else {
                break;
            }
        }
    }
    values
}

fn extract_chapter(order: usize, source_href: String, linear: bool, html: &str) -> ImportedChapter {
    let document = Html::parse_document(html);
    let mut candidates = Vec::new();
    for (source_index, node) in document.tree.nodes().enumerate() {
        let Some(element) = ElementRef::wrap(node) else {
            continue;
        };
        let Some(kind) = paragraph_kind(element.value().name(), element.value().attr("alt")) else {
            continue;
        };
        let nested_in_block = element.ancestors().skip(1).any(|ancestor| {
            ElementRef::wrap(ancestor).is_some_and(|parent| {
                paragraph_kind(parent.value().name(), parent.value().attr("alt")).is_some()
            })
        });
        if nested_in_block {
            continue;
        }
        let raw = if matches!(kind, ParagraphKind::ImageDescription) {
            element.value().attr("alt").unwrap_or_default().to_owned()
        } else {
            element.text().collect::<Vec<_>>().join(" ")
        };
        let text = normalize_whitespace(&raw);
        if !text.is_empty() {
            candidates.push((source_index, kind, text));
        }
    }
    candidates.sort_by_key(|(source_index, _, _)| *source_index);
    candidates.dedup_by(|left, right| left.0 == right.0 || left.2 == right.2);

    let title = candidates
        .iter()
        .find(|(_, kind, _)| matches!(kind, ParagraphKind::Heading))
        .map_or_else(
            || format!("Chapter {}", order + 1),
            |(_, _, text)| text.clone(),
        );

    let mut text = String::new();
    let mut paragraphs = Vec::with_capacity(candidates.len());
    for (source_index, kind, value) in candidates {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        let start_offset = text.len();
        text.push_str(&value);
        let end_offset = text.len();
        paragraphs.push(ImportedParagraph {
            source_index,
            kind,
            content_hash: blake3::hash(value.as_bytes()).to_hex().to_string(),
            text: value,
            start_offset,
            end_offset,
        });
    }
    let sanitized_html = ammonia::Builder::default()
        .rm_clean_content_tags(["script", "style"])
        .clean(html)
        .to_string();
    let content_hash = blake3::hash(text.as_bytes()).to_hex().to_string();
    ImportedChapter {
        order,
        source_href,
        title,
        linear,
        text,
        sanitized_html,
        paragraphs,
        content_hash,
    }
}

fn paragraph_kind(tag: &str, alt: Option<&str>) -> Option<ParagraphKind> {
    match tag {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => Some(ParagraphKind::Heading),
        "p" => Some(ParagraphKind::Paragraph),
        "li" => Some(ParagraphKind::ListItem),
        "blockquote" => Some(ParagraphKind::Quote),
        "pre" => Some(ParagraphKind::Preformatted),
        "img" if alt.is_some_and(|value| !value.trim().is_empty()) => {
            Some(ParagraphKind::ImageDescription)
        }
        _ => None,
    }
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_non_font_encryption_as_drm() {
        let font = r#"<EncryptionMethod Algorithm="http://www.idpf.org/2008/embedding"/>"#;
        let drm = r#"<EncryptionMethod Algorithm="http://www.w3.org/2001/04/xmlenc#aes256-cbc"/>"#;
        assert_eq!(
            encryption_algorithms(font),
            ["http://www.idpf.org/2008/embedding"]
        );
        assert_eq!(
            encryption_algorithms(drm),
            ["http://www.w3.org/2001/04/xmlenc#aes256-cbc"]
        );
    }

    #[test]
    fn extracts_ordered_semantic_text_and_stable_offsets() {
        let chapter = extract_chapter(
            0,
            "chapter.xhtml".to_owned(),
            true,
            "<html><body><h1>Hello</h1><p>Some <em>text</em>.</p><img alt='Map'/></body></html>",
        );
        assert_eq!(chapter.title, "Hello");
        assert_eq!(chapter.text, "Hello\n\nSome text .\n\nMap");
        assert_eq!(
            &chapter.text[chapter.paragraphs[1].start_offset..chapter.paragraphs[1].end_offset],
            "Some text ."
        );
    }

    #[test]
    fn rejects_parent_paths() {
        assert!(validate_entry_path("../escape", Some(Path::new("../escape"))).is_err());
    }
}
