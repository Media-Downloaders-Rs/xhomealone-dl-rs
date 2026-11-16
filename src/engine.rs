use crate::{
    Cancel, File, Listing, Site, sites,
    state::{Partial, State, representation_key, source_key},
    transport::{RetryOptions, SitePolicy, Transient, Transport, network_error},
};
use anyhow::{Result, bail};
use futures_util::{StreamExt, stream};
use md5::Md5;
use reqwest::header;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{BufWriter, Read, Write},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    List,
    Download,
    Refresh,
    Update,
}
pub struct RunOptions {
    pub site: Site,
    pub urls: Vec<String>,
    pub root: PathBuf,
    pub action: Action,
    pub workers: usize,
    pub password: Option<String>,
}
#[derive(Clone, Debug)]
pub enum FileOutcome {
    Downloaded,
    Skipped,
    Failed(String),
    Cancelled,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Summary {
    pub downloaded: usize,
    pub skipped: usize,
    pub failed: usize,
    pub pending: usize,
}
#[derive(Clone, Debug)]
pub enum Event {
    SourceChanged {
        previous_path: String,
        file: File,
    },
    Message(String),
    Listing {
        url: String,
        listing: Listing,
    },
    Progress {
        path: String,
        received: u64,
        total: Option<u64>,
    },
    FileDone {
        path: String,
        outcome: FileOutcome,
    },
    Finished(Summary),
}
pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;
pub struct Engine {
    pub options: RunOptions,
    pub state: Arc<State>,
    pub cancel: Cancel,
    pub sink: Sink,
    pub transport: Transport,
}
impl Engine {
    pub fn new(options: RunOptions, cancel: Cancel, sink: Sink) -> Result<Arc<Self>> {
        let callback = sink.clone();
        let transport = Transport::new(
            Arc::new(SitePolicy(options.site)),
            cancel.clone(),
            Arc::new(move |s| callback(Event::Message(s))),
            RetryOptions::default(),
        )?;
        Self::with_transport(options, cancel, sink, transport)
    }
    pub fn with_transport(
        mut options: RunOptions,
        cancel: Cancel,
        sink: Sink,
        transport: Transport,
    ) -> Result<Arc<Self>> {
        if !(1..=16).contains(&options.workers) {
            bail!("Elige entre 1 y 16 transferencias")
        }
        if options.action == Action::Update {
            bail!("Buscar nuevos no disponible para XHomeAlone")
        }
        options.urls = sites::inputs(options.site, &options.urls.join("\n"))?;
        let state = Arc::new(State::open(&options.root, options.site)?);
        state.import_legacy(options.site, &cancel)?;
        Ok(Arc::new(Self {
            options,
            state,
            cancel,
            sink,
            transport,
        }))
    }
    fn publish(
        &self,
        file: &File,
        partial: &std::path::Path,
        size: u64,
        digest: &str,
    ) -> Result<FileOutcome> {
        self.cancel.check()?;
        let target = self.state.target(&file.path)?;
        if !file.sha256.is_empty() && !file.sha256.eq_ignore_ascii_case(digest) {
            fs::write(partial, [])?;
            self.state.clear_partial(&file.path)?;
            return Err(Transient {
                message: "SHA-256 diferente; se reinicia el parcial".into(),
                after: None,
            }
            .into());
        }
        self.state
            .record(file, size, digest, crate::mtime(partial)?, "moving")?;
        if target.exists() {
            let folder = self.state.directory.join("replaced");
            fs::create_dir_all(&folder)?;
            let name = format!(
                "{}-{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos(),
                Sha256::digest(file.path.as_bytes())
            );
            let relative = folder
                .join(name)
                .strip_prefix(&self.state.root)?
                .to_string_lossy()
                .replace('\\', "/");
            let backup = crate::target(&self.state.root, &relative)?;
            fs::rename(&target, &backup)?;
            (self.sink)(Event::Message(format!(
                "Archivo anterior conservado: {}",
                backup.display()
            )));
        }
        fs::rename(partial, &target)?;
        self.state
            .record(file, size, digest, crate::mtime(&target)?, "complete")?;
        self.state.clear_partial(&file.path)?;
        Ok(FileOutcome::Downloaded)
    }
    pub async fn download_one(&self, file: &File) -> Result<FileOutcome> {
        self.download_one_limited(file, 4).await
    }
    async fn is_completed(&self, file: &File) -> Result<bool> {
        self.cancel.check()?;
        self.transport
            .policy
            .check(&Url::parse(&file.url)?, &file.post)?;
        let state = self.state.clone();
        let file = file.clone();
        let cancel = self.cancel.clone();
        tokio::task::spawn_blocking(move || state.completed(&file, &cancel)).await?
    }

    async fn download_one_limited(&self, file: &File, attempts: usize) -> Result<FileOutcome> {
        if self.is_completed(file).await? {
            return Ok(FileOutcome::Skipped);
        }
        self.transport
            .retry_limited(attempts, || self.transfer(file))
            .await
    }

    async fn download_xhomealone(&self, mut file: File) -> (File, Result<FileOutcome>) {
        match self.is_completed(&file).await {
            Ok(true) => return (file, Ok(FileOutcome::Skipped)),
            Ok(false) => {}
            Err(error) => return (file, Err(error)),
        }
        let post = file.post.clone();
        let cancel = self.cancel.clone();
        let sink = self.sink.clone();
        let fresh = tokio::task::spawn_blocking(move || -> Result<String> {
            let log: crate::Log = Arc::new(move |message| sink(Event::Message(message)));
            let mut browser = crate::browser::BrowserSession::launch(true, cancel.clone(), log)?;
            sites::xhomealone::browser_source(&mut browser, &post, &cancel)
        })
        .await;
        let url = match fresh {
            Ok(Ok(url)) => url,
            Ok(Err(error)) => return (file, Err(error)),
            Err(error) => return (file, Err(error.into())),
        };
        (self.sink)(Event::Message(format!(
            "Fuente MP4 del navegador lista: {}",
            file.id
        )));
        file.url = url;
        (self.sink)(Event::SourceChanged {
            previous_path: file.path.clone(),
            file: file.clone(),
        });
        let outcome = self.download_one(&file).await;
        (file, outcome)
    }
    pub async fn download_with_fallback(&self, file: File) -> (File, Result<FileOutcome>) {
        self.download_xhomealone(file).await
    }
    async fn transfer(&self, file: &File) -> Result<FileOutcome> {
        self.cancel.check()?;
        let source = representation_key(file)?;
        let target = self.state.target(&file.path)?;
        fs::create_dir_all(
            target
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Destino sin carpeta"))?,
        )?;
        let partial = self.state.partial_path(&file.path)?;
        let info = self.state.partial(&file.path)?;
        let mut offset = partial.metadata().map(|m| m.len()).unwrap_or(0);
        let compatible = info.as_ref().is_some_and(|p| {
            p.source == source
                && !p.validator.is_empty()
                && !p.validator.starts_with("W/")
                && p.total.is_none_or(|n| offset <= n)
                && file
                    .expected_size
                    .is_none_or(|n| p.total.is_none_or(|old| n == old))
        });
        if !compatible {
            offset = 0;
        }
        if offset > 0 && info.as_ref().and_then(|p| p.total) == Some(offset) {
            let (digest, md5) = prefix_hashes(&partial, &self.cancel, !file.md5.is_empty())?;
            if !file.md5.is_empty()
                && format!("{:x}", md5.finalize()) != file.md5.to_ascii_lowercase()
            {
                fs::write(&partial, [])?;
                self.state.clear_partial(&file.path)?;
                return Err(Transient {
                    message: "MD5 diferente; se reinicia el parcial".into(),
                    after: None,
                }
                .into());
            }
            return self.publish(file, &partial, offset, &format!("{:x}", digest.finalize()));
        }
        let range = if offset > 0 {
            Some((offset, info.as_ref().unwrap().validator.clone()))
        } else {
            None
        };
        let mut response = self.transport.request(&file.url, &file.post, range).await?;
        let redirected = response.url() != &Url::parse(&file.url)?;
        if redirected {
            offset = 0;
        }
        let mime = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !(mime.starts_with("image/")
            || mime.starts_with("video/")
            || mime == "application/octet-stream")
        {
            bail!("El servidor no devolvió un archivo multimedia")
        }
        let mut segment_length = None;
        let total = match response.status().as_u16() {
            200 => {
                offset = 0;
                response.content_length()
            }
            206 => {
                let value = response
                    .headers()
                    .get(header::CONTENT_RANGE)
                    .and_then(|s| s.to_str().ok())
                    .unwrap_or("");
                let pattern = regex::Regex::new(r"^bytes (\d+)-(\d+)/(\d+)$")?;
                let c = pattern
                    .captures(value)
                    .ok_or_else(|| anyhow::anyhow!("Rango de descarga inválido"))?;
                let start: u64 = c[1].parse()?;
                let end: u64 = c[2].parse()?;
                let total: u64 = c[3].parse()?;
                if start != offset
                    || end < start
                    || end >= total
                    || info
                        .as_ref()
                        .and_then(|p| p.total)
                        .is_some_and(|old| offset > 0 && old != total)
                {
                    bail!("El rango no coincide con el parcial")
                };
                let length = end - start + 1;
                if response.content_length().is_some_and(|n| n != length) {
                    bail!("Content-Length contradice Content-Range")
                }
                segment_length = Some(length);
                Some(total)
            }
            status => bail!("Estado HTTP inesperado {status}"),
        };
        if let (Some(wanted), Some(actual)) = (file.expected_size, total)
            && wanted != actual
        {
            bail!("Cambió el tamaño del archivo; actualiza el listado")
        }
        let total = total.or(file.expected_size);
        let current = response
            .headers()
            .get(header::ETAG)
            .and_then(|s| s.to_str().ok())
            .filter(|s| !s.starts_with("W/"))
            .or_else(|| {
                response
                    .headers()
                    .get(header::LAST_MODIFIED)
                    .and_then(|s| s.to_str().ok())
            })
            .unwrap_or("")
            .to_string();
        if offset > 0
            && !current.is_empty()
            && info.as_ref().is_some_and(|p| p.validator != current)
        {
            fs::write(&partial, [])?;
            self.state.clear_partial(&file.path)?;
            return Err(Transient {
                message: "Cambió el identificador; se reinicia el archivo".into(),
                after: None,
            }
            .into());
        }
        let validator = if redirected {
            // Redirected representations are downloaded from zero on retry.
            // Never label their bytes as resumable at the original URL.
            String::new()
        } else if current.is_empty() && offset > 0 {
            info.as_ref().unwrap().validator.clone()
        } else {
            current
        };
        let (mut digest, mut md5) = if offset > 0 {
            prefix_hashes(&partial, &self.cancel, !file.md5.is_empty())?
        } else {
            (Sha256::new(), Md5::new())
        };
        let mut writer = BufWriter::with_capacity(
            256 * 1024,
            OpenOptions::new()
                .write(true)
                .create(true)
                .append(offset > 0)
                .truncate(offset == 0)
                .open(&partial)?,
        );
        // The bytes must be initialized before attaching a new representation.
        // A failed open must leave the old association intact.
        self.state.save_partial(
            &file.path,
            &Partial {
                source,
                validator,
                total,
            },
        )?;
        let mut size = offset;
        let mut received = 0u64;
        let mut progress = Instant::now();
        (self.sink)(Event::Progress {
            path: file.path.clone(),
            received: size,
            total,
        });
        while let Some(chunk) = self
            .cancel
            .race(response.chunk())
            .await?
            .map_err(network_error)?
        {
            self.cancel.check()?;
            received = received
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| anyhow::anyhow!("Tamaño de respuesta excesivo"))?;
            if segment_length.is_some_and(|n| received > n) {
                self.state.clear_partial(&file.path)?;
                bail!("El cuerpo excede el rango anunciado")
            }
            writer.write_all(&chunk)?;
            digest.update(&chunk);
            if !file.md5.is_empty() {
                md5.update(&chunk);
            }
            size += chunk.len() as u64;
            if total.is_some_and(|n| size > n) {
                bail!("El servidor envió más bytes de los anunciados")
            }
            if progress.elapsed().as_millis() >= 500 {
                (self.sink)(Event::Progress {
                    path: file.path.clone(),
                    received: size,
                    total,
                });
                progress = Instant::now();
            }
        }
        writer.flush()?;
        drop(writer);
        if segment_length.is_some_and(|n| received != n) {
            self.state.clear_partial(&file.path)?;
            return Err(Transient {
                message: "El cuerpo no completa el rango anunciado".into(),
                after: None,
            }
            .into());
        }
        if (size == 0 && file.expected_size != Some(0)) || total.is_some_and(|n| n != size) {
            return Err(Transient {
                message: "Descarga incompleta".into(),
                after: None,
            }
            .into());
        }
        if !file.md5.is_empty() && format!("{:x}", md5.finalize()) != file.md5.to_ascii_lowercase()
        {
            fs::write(&partial, [])?;
            self.state.clear_partial(&file.path)?;
            return Err(Transient {
                message: "MD5 diferente; se reintentará desde cero".into(),
                after: None,
            }
            .into());
        }
        (self.sink)(Event::Progress {
            path: file.path.clone(),
            received: size,
            total: Some(size),
        });
        self.publish(file, &partial, size, &format!("{:x}", digest.finalize()))
    }
    async fn discover(&self, url: &str) -> Result<Listing> {
        sites::xhomealone::resolve(url, &self.cancel, &|page: String| async move {
            (self.sink)(Event::Message(format!("Leyendo {page}")));
            self.transport.html(&page).await
        })
        .await
    }
    pub async fn run(self: Arc<Self>) -> Result<Summary> {
        self.run_selected(None).await
    }

    pub async fn run_selected(
        self: Arc<Self>,
        selected: Option<Arc<HashSet<String>>>,
    ) -> Result<Summary> {
        let mut summary = Summary::default();
        for (index, url) in self.options.urls.iter().enumerate() {
            self.cancel.check()?;
            (self.sink)(Event::Message(format!(
                "Enlace {}/{}: {url}",
                index + 1,
                self.options.urls.len()
            )));
            let listing = match self.discover(url).await {
                Ok(listing) => listing,
                Err(error) => {
                    self.cancel.check()?;
                    summary.failed += 1;
                    (self.sink)(Event::Message(format!("ERROR: {error}")));
                    continue;
                }
            };
            (self.sink)(Event::Listing {
                url: url.clone(),
                listing: listing.clone(),
            });
            summary.pending += listing.pending.len();
            if self.options.action != Action::Download {
                continue;
            }
            let mut seen = HashMap::new();
            let mut files = Vec::new();
            for file in listing.files {
                if selected
                    .as_ref()
                    .is_some_and(|paths| !paths.contains(&file.path))
                {
                    continue;
                }
                let key = file.path.to_lowercase();
                let source = source_key(&file)?;
                if let Some(previous) = seen.get(&key) {
                    if previous != &source {
                        summary.failed += 1;
                        (self.sink)(Event::FileDone {
                            path: file.path,
                            outcome: FileOutcome::Failed(
                                "Dos archivos distintos usan el mismo destino".into(),
                            ),
                        });
                    }
                    continue;
                }
                seen.insert(key, source);
                files.push(file);
            }
            let jobs = stream::iter(files.into_iter().map(|file| {
                let engine = self.clone();
                async move { engine.download_with_fallback(file).await }
            }))
            .buffer_unordered(self.options.workers.min(2));
            tokio::pin!(jobs);
            while let Some((file, result)) = jobs.next().await {
                self.cancel.check()?;
                let outcome = match result {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        summary.failed += 1;
                        FileOutcome::Failed(error.to_string())
                    }
                };
                match outcome {
                    FileOutcome::Downloaded => summary.downloaded += 1,
                    FileOutcome::Skipped => summary.skipped += 1,
                    _ => {}
                }
                (self.sink)(Event::FileDone {
                    path: file.path,
                    outcome,
                });
            }
        }
        (self.sink)(Event::Finished(summary.clone()));
        Ok(summary)
    }
}

fn prefix_hashes(path: &std::path::Path, cancel: &Cancel, with_md5: bool) -> Result<(Sha256, Md5)> {
    let mut input = std::fs::File::open(path)?;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut digest = Sha256::new();
    let mut md5 = Md5::new();
    loop {
        cancel.check()?;
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
        if with_md5 {
            md5.update(&buffer[..n]);
        }
    }
    Ok((digest, md5))
}
