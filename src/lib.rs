pub mod browser;

pub mod engine;
pub mod global;
#[cfg(feature = "gui")]
pub mod gui;
pub mod settings;
pub mod sites;
pub mod state;
pub mod transport;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Site {
    XHomeAlone,
}

impl Site {
    pub fn key(self) -> &'static str {
        "xhomealone"
    }
    pub fn title(self) -> String {
        "XHomeAlone DL · Rust".into()
    }
    pub fn parse(value: &str) -> Result<Self> {
        if value == "xhomealone" {
            Ok(Self::XHomeAlone)
        } else {
            bail!("Sitio desconocido: {value}")
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct File {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub post: String,
    pub url: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    #[serde(default, alias = "size")]
    pub expected_size: Option<u64>,
    #[serde(default)]
    pub md5: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Box<File>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Listing {
    pub title: String,
    pub files: Vec<File>,
    #[serde(default)]
    pub pending: Vec<Pending>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Pending {
    pub url: String,
    pub error: String,
}

#[derive(Clone, Default)]
pub struct Cancel(pub Arc<AtomicBool>);
impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("Cancelado; el progreso se conserva.")
        }
        Ok(())
    }
    pub async fn race<T>(&self, future: impl std::future::Future<Output = T>) -> Result<T> {
        self.check()?;
        tokio::select! {value=future=>Ok(value),_=async{loop{if self.is_cancelled(){break}tokio::time::sleep(std::time::Duration::from_millis(50)).await;}}=>bail!("Cancelado; el progreso se conserva.")}
    }
    pub async fn sleep(&self, duration: std::time::Duration) -> Result<()> {
        let end = std::time::Instant::now() + duration;
        loop {
            self.check()?;
            let now = std::time::Instant::now();
            if now >= end {
                return Ok(());
            }
            tokio::time::sleep((end - now).min(std::time::Duration::from_millis(150))).await;
        }
    }
}
pub type Log = Arc<dyn Fn(String) + Send + Sync>;
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
pub fn mtime(path: &Path) -> Result<i64> {
    Ok(path
        .metadata()?
        .modified()?
        .duration_since(UNIX_EPOCH)?
        .as_nanos()
        .try_into()?)
}

pub fn safe_name(value: &str) -> String {
    let name: String = value
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .take(100)
        .collect();
    let mut name = name.trim_matches(['.', ' ']).to_string();
    if name.is_empty() {
        name = "sin-nombre".into()
    }
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
    {
        name.insert(0, '_')
    }
    name
}

pub fn target(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    if relative.contains('\\')
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("Ruta de destino inválida: {relative}")
    }
    let mut result = root.to_path_buf();
    for part in path.components() {
        result.push(part);
        if result.exists() {
            let actual = result.canonicalize()?;
            if !actual.starts_with(root.canonicalize()?) {
                bail!("La ruta sale de la carpeta de descargas.")
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portable_names() {
        assert_eq!(safe_name("CON"), "_CON");
        assert_eq!(safe_name("../E girl?"), "_E girl_");
        assert_eq!(safe_name("日本語"), "日本語");
    }
    #[test]
    fn no_traversal() {
        let t = tempfile::tempdir().unwrap();
        assert!(target(t.path(), "../escape").is_err());
        assert!(target(t.path(), "a/../../x").is_err());
        assert!(target(t.path(), "album/photo.jpg").is_ok());
    }
}
