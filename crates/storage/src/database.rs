use std::{
    fs::{File, OpenOptions},
    path::Path,
    str::FromStr,
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use fs2::FileExt;
use sqlx::{
    SqlitePool,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use tracing::info;
use uuid::Uuid;

use crate::{AppPaths, Result, StorageError, harden_private_file, repositories::Repositories};

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

#[derive(Clone, Debug)]
pub struct Database {
    pool: SqlitePool,
    paths: AppPaths,
    _writer_lock: Arc<File>,
}

impl Database {
    pub async fn open_default() -> Result<Self> {
        Self::open(AppPaths::discover()?).await
    }

    pub async fn open_in(root: impl AsRef<Path>) -> Result<Self> {
        Self::open(AppPaths::from_root(root)).await
    }

    pub async fn open(paths: AppPaths) -> Result<Self> {
        paths.ensure().await?;
        let writer_lock = acquire_writer_lock(&paths)?;
        harden_private_file(&paths.writer_lock).await?;
        backup_existing_database(&paths).await?;

        let database_url = format!("sqlite://{}", paths.database.to_string_lossy());
        let options = SqliteConnectOptions::from_str(&database_url)?
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .min_connections(1)
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(15))
            .connect_with(options)
            .await?;

        if let Err(error) = harden_private_file(&paths.database).await {
            pool.close().await;
            return Err(error);
        }

        if let Err(error) = MIGRATOR.run(&pool).await {
            pool.close().await;
            return Err(error.into());
        }
        sqlx::query("PRAGMA optimize").execute(&pool).await?;

        info!(diagnostic_code = "storage.database.opened", database = %paths.database.display(), "opened AudiobookAI database");
        Ok(Self {
            pool,
            paths,
            _writer_lock: Arc::new(writer_lock),
        })
    }

    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    #[must_use]
    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    #[must_use]
    pub fn repositories(&self) -> Repositories {
        Repositories::new(self.pool.clone())
    }

    pub async fn close(self) {
        self.pool.close().await;
    }
}

fn acquire_writer_lock(paths: &AppPaths) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&paths.writer_lock)?;
    file.try_lock_exclusive()
        .map_err(|_| StorageError::AlreadyRunning(paths.writer_lock.clone()))?;
    Ok(file)
}

async fn backup_existing_database(paths: &AppPaths) -> Result<()> {
    let metadata = match tokio::fs::metadata(&paths.database).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() == 0 {
        return Ok(());
    }

    let timestamp = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    let backup = paths.backups.join(format!(
        "audiobookai-{timestamp}-{}.sqlite3",
        Uuid::new_v4()
    ));
    tokio::fs::copy(&paths.database, &backup).await?;
    harden_private_file(&backup).await?;
    info!(diagnostic_code = "storage.database.backup.created", backup = %backup.display(), "created pre-migration database backup");
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("file metadata")
            .permissions()
            .mode()
            & 0o777
    }

    #[tokio::test]
    async fn database_lock_and_backups_are_owner_only() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = temporary.path().join("data");

        let database = Database::open_in(&root).await.expect("first database open");
        let paths = database.paths().clone();
        database.close().await;

        assert_eq!(mode(&paths.database), 0o600);
        assert_eq!(mode(&paths.writer_lock), 0o600);

        let reopened = Database::open_in(&root)
            .await
            .expect("second database open");
        let backups = std::fs::read_dir(&paths.backups)
            .expect("backup directory")
            .map(|entry| entry.expect("backup entry").path())
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1);
        assert_eq!(mode(&backups[0]), 0o600);
        reopened.close().await;
    }
}
