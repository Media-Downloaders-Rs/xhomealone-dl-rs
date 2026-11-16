//! Optional local routing preferences. No sibling repository is required.
use crate::Site;
const SITES: [Site; 1] = [Site::XHomeAlone];
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Destinations {
    pub base: PathBuf,
    #[serde(default)]
    pub overrides: HashMap<Site, PathBuf>,
}
impl Destinations {
    pub fn new(base: PathBuf) -> Self {
        Self {
            base,
            overrides: HashMap::new(),
        }
    }
    pub fn root(&self, site: Site) -> PathBuf {
        self.overrides
            .get(&site)
            .cloned()
            .unwrap_or_else(|| self.base.join(site.key()))
    }
    pub fn validate(&self, sites: &[Site]) -> Result<()> {
        if !self.base.is_absolute() {
            bail!("El destino base debe ser una ruta absoluta")
        }
        let mut checked: Vec<(Site, String)> = Vec::new();
        for &site in sites {
            if checked.iter().any(|(s, _)| *s == site) {
                continue;
            }
            let key = physical_key(&self.root(site))?;
            for (other, previous) in &checked {
                if key == *previous
                    || key.starts_with(&format!("{previous}/"))
                    || previous.starts_with(&format!("{key}/"))
                {
                    bail!(
                        "Los destinos de {} y {} coinciden o están uno dentro del otro",
                        site.key(),
                        other.key()
                    )
                }
            }
            checked.push((site, key));
        }
        Ok(())
    }
}
fn physical_key(path: &Path) -> Result<String> {
    if !path.is_absolute() {
        bail!("Cada destino debe ser una ruta absoluta")
    }
    let mut lexical = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !lexical.pop() {
                    bail!("Destino fuera de su raíz")
                }
            }
            _ => lexical.push(part.as_os_str()),
        }
    }
    if lexical.exists() && !lexical.is_dir() {
        bail!("El destino no es una carpeta")
    }
    let mut ancestor = lexical.clone();
    let mut tail = Vec::new();
    while !ancestor.exists() {
        tail.push(
            ancestor
                .file_name()
                .context("Destino sin raíz accesible")?
                .to_os_string(),
        );
        if !ancestor.pop() {
            bail!("Destino sin raíz accesible")
        }
    }
    if !ancestor.is_dir() {
        bail!("Un componente del destino no es una carpeta")
    }
    let mut resolved = ancestor.canonicalize()?;
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_lowercase())
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preferences {
    pub version: u8,
    pub workers: usize,
    pub destinations: Destinations,
}
impl Preferences {
    pub fn default_for(app_dir: &Path) -> Self {
        let destinations = Destinations::new(app_dir.join("downloads"));

        Self {
            version: 1,
            workers: 4,
            destinations,
        }
    }
    fn validate(&self) -> Result<()> {
        if self.version != 1 || !(1..=16).contains(&self.workers) {
            bail!("Preferencias no válidas o de otra versión")
        }
        self.destinations.validate(&SITES)
    }
}
pub fn load(path: &Path) -> Result<Preferences> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        bail!("Archivo de preferencias demasiado grande")
    }
    let preferences: Preferences = serde_json::from_slice(&bytes)?;
    preferences.validate()?;
    Ok(preferences)
}
pub fn save(path: &Path, preferences: &Preferences) -> Result<()> {
    preferences.validate()?;
    let parent = path.parent().context("Preferencias sin carpeta")?;
    std::fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(staged.as_file_mut(), preferences)?;
    writeln!(staged.as_file_mut())?;
    staged.as_file().sync_all()?;
    staged.persist(path).map_err(|e| e.error)?;
    Ok(())
}
