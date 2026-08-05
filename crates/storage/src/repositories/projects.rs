use audiobookai_core::{Book, BookId, Chapter, Paragraph, Project, ProjectId, Validate};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};

use crate::{Result, StorageError};

use super::util::{decode, encode, enum_text};

#[derive(Clone, Debug)]
pub struct ProjectRepository {
    pool: SqlitePool,
}

impl ProjectRepository {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert_book(&self, book: &Book) -> Result<()> {
        book.validate()?;
        let result = sqlx::query(
            "INSERT INTO books (id, managed_epub_path, source_hash, imported_at, payload) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(book.id.to_string())
        .bind(&book.managed_epub_path)
        .bind(&book.source_fingerprint.digest)
        .bind(book.imported_at.to_rfc3339())
        .bind(encode(book)?)
        .execute(&self.pool)
        .await;
        map_insert(result, "book", book.id.to_string())?;
        Ok(())
    }

    pub async fn get_book(&self, id: BookId) -> Result<Option<Book>> {
        let payload = sqlx::query_scalar::<_, String>("SELECT payload FROM books WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        payload.as_deref().map(decode).transpose()
    }

    pub async fn insert_project(&self, project: &Project) -> Result<()> {
        project.validate()?;
        let result = sqlx::query(
            "INSERT INTO projects \
             (id, book_id, name, status, created_at, updated_at, revision, payload) \
             VALUES (?, ?, ?, ?, ?, ?, 0, ?)",
        )
        .bind(project.id.to_string())
        .bind(project.book_id.to_string())
        .bind(&project.name)
        .bind(enum_text(&project.status)?)
        .bind(project.created_at.to_rfc3339())
        .bind(project.updated_at.to_rfc3339())
        .bind(encode(project)?)
        .execute(&self.pool)
        .await;
        map_insert(result, "project", project.id.to_string())?;
        Ok(())
    }

    pub async fn create_import(
        &self,
        book: &Book,
        project: &Project,
        chapters: &[Chapter],
        paragraphs: &[Paragraph],
    ) -> Result<()> {
        book.validate()?;
        project.validate()?;
        if project.book_id != book.id {
            return Err(StorageError::InvalidData(
                "project book id does not match imported book".into(),
            ));
        }
        for paragraph in paragraphs {
            paragraph.validate()?;
        }

        let mut tx = self.pool.begin().await?;
        insert_book_tx(&mut tx, book).await?;
        insert_project_tx(&mut tx, project).await?;
        for chapter in chapters {
            if chapter.book_id != book.id {
                return Err(StorageError::InvalidData(
                    "chapter belongs to a different book".into(),
                ));
            }
            sqlx::query(
                "INSERT INTO chapters (id, book_id, ordinal, selected, text_hash, payload) \
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(chapter.id.to_string())
            .bind(chapter.book_id.to_string())
            .bind(i64::from(chapter.ordinal))
            .bind(chapter.selected)
            .bind(&chapter.text_hash)
            .bind(encode(chapter)?)
            .execute(&mut *tx)
            .await?;
        }
        for paragraph in paragraphs {
            sqlx::query(
                "INSERT INTO paragraphs (id, chapter_id, ordinal, content_hash, payload) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(paragraph.id.to_string())
            .bind(paragraph.chapter_id.to_string())
            .bind(i64::from(paragraph.ordinal))
            .bind(&paragraph.content_hash)
            .bind(encode(paragraph)?)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_project(&self, id: ProjectId) -> Result<Option<Project>> {
        let payload = sqlx::query_scalar::<_, String>("SELECT payload FROM projects WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        payload.as_deref().map(decode).transpose()
    }

    pub async fn list_projects(&self, include_archived: bool) -> Result<Vec<Project>> {
        let rows = if include_archived {
            sqlx::query("SELECT payload FROM projects ORDER BY updated_at DESC")
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query(
                "SELECT payload FROM projects WHERE status <> 'archived' ORDER BY updated_at DESC",
            )
            .fetch_all(&self.pool)
            .await?
        };
        rows.into_iter()
            .map(|row| decode(row.get::<&str, _>("payload")))
            .collect()
    }

    pub async fn update_project(&self, project: &Project, expected_revision: u64) -> Result<u64> {
        project.validate()?;
        let next_revision = expected_revision.saturating_add(1);
        let result = sqlx::query(
            "UPDATE projects SET name = ?, status = ?, updated_at = ?, revision = ?, payload = ? \
             WHERE id = ? AND revision = ?",
        )
        .bind(&project.name)
        .bind(enum_text(&project.status)?)
        .bind(project.updated_at.to_rfc3339())
        .bind(i64::try_from(next_revision).unwrap_or(i64::MAX))
        .bind(encode(project)?)
        .bind(project.id.to_string())
        .bind(i64::try_from(expected_revision).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            let exists = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM projects WHERE id = ?")
                .bind(project.id.to_string())
                .fetch_one(&self.pool)
                .await?
                > 0;
            return Err(if exists {
                StorageError::StaleRevision {
                    entity: "project",
                    id: project.id.to_string(),
                }
            } else {
                StorageError::NotFound {
                    entity: "project",
                    id: project.id.to_string(),
                }
            });
        }
        Ok(next_revision)
    }

    pub async fn list_chapters(&self, book_id: BookId) -> Result<Vec<Chapter>> {
        let rows = sqlx::query("SELECT payload FROM chapters WHERE book_id = ? ORDER BY ordinal")
            .bind(book_id.to_string())
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| decode(row.get::<&str, _>("payload")))
            .collect()
    }

    pub async fn list_paragraphs(
        &self,
        chapter_id: audiobookai_core::ChapterId,
    ) -> Result<Vec<Paragraph>> {
        let rows =
            sqlx::query("SELECT payload FROM paragraphs WHERE chapter_id = ? ORDER BY ordinal")
                .bind(chapter_id.to_string())
                .fetch_all(&self.pool)
                .await?;
        rows.into_iter()
            .map(|row| decode(row.get::<&str, _>("payload")))
            .collect()
    }
}

async fn insert_book_tx(tx: &mut Transaction<'_, Sqlite>, book: &Book) -> Result<()> {
    sqlx::query(
        "INSERT INTO books (id, managed_epub_path, source_hash, imported_at, payload) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(book.id.to_string())
    .bind(&book.managed_epub_path)
    .bind(&book.source_fingerprint.digest)
    .bind(book.imported_at.to_rfc3339())
    .bind(encode(book)?)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_project_tx(tx: &mut Transaction<'_, Sqlite>, project: &Project) -> Result<()> {
    sqlx::query(
        "INSERT INTO projects \
         (id, book_id, name, status, created_at, updated_at, revision, payload) \
         VALUES (?, ?, ?, ?, ?, ?, 0, ?)",
    )
    .bind(project.id.to_string())
    .bind(project.book_id.to_string())
    .bind(&project.name)
    .bind(enum_text(&project.status)?)
    .bind(project.created_at.to_rfc3339())
    .bind(project.updated_at.to_rfc3339())
    .bind(encode(project)?)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn map_insert(
    result: std::result::Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error>,
    entity: &'static str,
    id: String,
) -> Result<sqlx::sqlite::SqliteQueryResult> {
    result.map_err(|error| {
        if StorageError::is_unique_violation(&error) {
            StorageError::Conflict { entity, id }
        } else {
            error.into()
        }
    })
}
