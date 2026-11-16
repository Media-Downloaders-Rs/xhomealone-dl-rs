use crate::{Cancel, File, Listing, Site};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub struct State {
    pub root: PathBuf,
    pub path: PathBuf,
    pub directory: PathBuf,
    db: Mutex<Connection>,
    _lock: std::fs::File,
}
#[derive(Clone, Debug)]
pub struct Partial {
    pub source: String,
    pub validator: String,
    pub total: Option<u64>,
}
impl State {
    pub fn import_legacy(&self, _site: Site, _cancel: &Cancel) -> Result<()> {
        Ok(())
    }
    pub fn post(&self, url: &str) -> Result<Option<Vec<String>>> {
        let row: Option<(String, f64)> = self
            .connection()?
            .query_row(
                "SELECT data,saved_at FROM rust_posts WHERE url=?",
                [url],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((value, saved)) if (0.0..86400.0).contains(&(crate::now() - saved)) => {
                Ok(Some(serde_json::from_str(&value)?))
            }
            _ => Ok(None),
        }
    }
    /// Durable provider checkpoints are not expiring HTTP page caches.
    pub fn post_snapshot(&self, url: &str) -> Result<Option<Vec<String>>> {
        let value: Option<String> = self
            .connection()?
            .query_row("SELECT data FROM rust_posts WHERE url=?", [url], |row| {
                row.get(0)
            })
            .optional()?;
        value
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    pub fn save_post(&self, url: &str, media: &[String]) -> Result<()> {
        self.connection()?.execute(
            "INSERT OR REPLACE INTO rust_posts VALUES (?,?,?)",
            params![url, serde_json::to_string(media)?, crate::now()],
        )?;
        Ok(())
    }

    pub fn open(root: &Path, site: Site) -> Result<Self> {
        fs::create_dir_all(root)?;
        let root = root.canonicalize()?;
        let directory = root.join(format!(".{}-dl", site.key()));
        fs::create_dir_all(&directory)?;
        if !directory.canonicalize()?.starts_with(&root) {
            bail!("El directorio de seguimiento sale del destino.")
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("rust.lock"))?;
        lock.try_lock_exclusive()
            .context("Ya hay una descarga Rust activa en esta carpeta")?;
        let path = directory.join("state.sqlite3");
        let existed = path.exists();
        let db = Connection::open(&path)?;
        db.busy_timeout(std::time::Duration::from_secs(30))?;
        let has_schema: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='rust_meta')",
            [],
            |r| r.get(0),
        )?;
        if existed && !has_schema {
            let backup = directory.join("rust-backups");
            fs::create_dir_all(&backup)?;
            db.backup(
                "main",
                backup.join(format!(
                    "before-rust-{}.sqlite3",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_nanos()
                )),
                None,
            )?;
        }
        db.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS rust_meta (key TEXT PRIMARY KEY,value TEXT NOT NULL);
            INSERT OR IGNORE INTO rust_meta VALUES ('schema_version','1');
            CREATE TABLE IF NOT EXISTS rust_downloads (path_key TEXT PRIMARY KEY,source TEXT NOT NULL,size INTEGER NOT NULL,sha256 TEXT NOT NULL,mtime_ns INTEGER NOT NULL,status TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS rust_listings (url TEXT PRIMARY KEY,data TEXT NOT NULL,saved_at REAL NOT NULL);
            CREATE TABLE IF NOT EXISTS rust_parts (path_key TEXT PRIMARY KEY,source TEXT NOT NULL,validator TEXT NOT NULL,total INTEGER);
            CREATE TABLE IF NOT EXISTS rust_posts (url TEXT PRIMARY KEY,data TEXT NOT NULL,saved_at REAL NOT NULL);")?;
        Ok(Self {
            root,
            path,
            directory,
            db: Mutex::new(db),
            _lock: lock,
        })
    }
    pub(crate) fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.db
            .lock()
            .map_err(|_| anyhow::anyhow!("Error de bloqueo SQLite"))
    }
    pub fn target(&self, relative: &str) -> Result<PathBuf> {
        let path = crate::target(&self.root, relative)?;
        if path.starts_with(&self.directory) {
            bail!("Un medio no puede escribirse en la carpeta de seguimiento")
        }
        Ok(path)
    }
    pub fn listing(&self, url: &str) -> Result<Option<Listing>> {
        let value: Option<String> = self
            .connection()?
            .query_row("SELECT data FROM rust_listings WHERE url=?", [url], |r| {
                r.get(0)
            })
            .optional()?;
        value
            .map(|v| serde_json::from_str(&v).map_err(Into::into))
            .transpose()
    }
    pub fn save_listing(&self, url: &str, listing: &Listing) -> Result<()> {
        self.connection()?.execute(
            "INSERT OR REPLACE INTO rust_listings VALUES (?,?,?)",
            params![url, serde_json::to_string(listing)?, crate::now()],
        )?;
        Ok(())
    }
    pub fn completed(&self, file: &File, cancel: &Cancel) -> Result<bool> {
        cancel.check()?;
        let row: Option<(String, i64, String, i64, String)> = self
            .connection()?
            .query_row(
                "SELECT source,size,sha256,mtime_ns,status FROM rust_downloads WHERE path_key=?",
                [file.path.to_lowercase()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((source, size, digest, stamp, status)) = row else {
            return Ok(false);
        };
        let path = self.target(&file.path)?;
        if (!file.sha256.is_empty() && !digest.eq_ignore_ascii_case(&file.sha256))
            || source != source_key(file)?
            || !path.is_file()
            || size < 0
            || file
                .expected_size
                .is_some_and(|expected| expected != size as u64)
            || path.metadata()?.len() != size as u64
        {
            return Ok(false);
        }
        let current = crate::mtime(&path)?;
        if stamp != current || status != "complete" {
            if hash_file(&path, cancel)? != digest {
                return Ok(false);
            }
            self.record(file, size as u64, &digest, current, "complete")?;
        }
        Ok(true)
    }
    pub fn record(
        &self,
        file: &File,
        size: u64,
        digest: &str,
        mtime: i64,
        status: &str,
    ) -> Result<()> {
        if digest.len() != 64 || !digest.bytes().all(|c| c.is_ascii_hexdigit()) {
            bail!("Registro SHA-256 inválido")
        }
        let size = i64::try_from(size)?;
        self.connection()?.execute(
            "INSERT OR REPLACE INTO rust_downloads VALUES (?,?,?,?,?,?)",
            params![
                file.path.to_lowercase(),
                source_key(file)?,
                size,
                digest,
                mtime,
                status
            ],
        )?;
        Ok(())
    }
    pub fn partial_path(&self, relative: &str) -> Result<PathBuf> {
        let parts = self.directory.join("rust-parts");
        fs::create_dir_all(&parts)?;
        if !parts.canonicalize()?.starts_with(&self.root) {
            bail!("La carpeta de parciales sale del destino")
        }
        Ok(parts.join(format!(
            "{:x}.part",
            Sha256::digest(relative.to_lowercase().as_bytes())
        )))
    }
    pub fn partial(&self, relative: &str) -> Result<Option<Partial>> {
        Ok(self
            .connection()?
            .query_row(
                "SELECT source,validator,total FROM rust_parts WHERE path_key=?",
                [relative.to_lowercase()],
                |r| {
                    let total: Option<i64> = r.get(2)?;
                    Ok(Partial {
                        source: r.get(0)?,
                        validator: r.get(1)?,
                        total: total.and_then(|n| u64::try_from(n).ok()),
                    })
                },
            )
            .optional()?)
    }
    pub fn save_partial(&self, relative: &str, value: &Partial) -> Result<()> {
        let total = value.total.map(i64::try_from).transpose()?;
        self.connection()?.execute(
            "INSERT OR REPLACE INTO rust_parts VALUES (?,?,?,?)",
            params![
                relative.to_lowercase(),
                value.source,
                value.validator,
                total
            ],
        )?;
        Ok(())
    }
    pub fn clear_partial(&self, relative: &str) -> Result<()> {
        self.connection()?.execute(
            "DELETE FROM rust_parts WHERE path_key=?",
            [relative.to_lowercase()],
        )?;
        Ok(())
    }
}

pub fn source_key(file: &File) -> Result<String> {
    crate::sites::xhomealone::validate_media(&url::Url::parse(&file.url)?, &file.post)?;
    Ok(format!("xhomealone:{}", file.id))
}
pub fn representation_key(file: &File) -> Result<String> {
    Ok(format!("{}:{:?}", source_key(file)?, file.expected_size))
}
pub fn hash_file(path: &Path, cancel: &Cancel) -> Result<String> {
    let mut source = std::fs::File::open(path)?;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut digest = Sha256::new();
    loop {
        cancel.check()?;
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
