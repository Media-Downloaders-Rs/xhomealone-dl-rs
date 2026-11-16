pub mod xhomealone;
use crate::Site;
use anyhow::{Result, bail};
use std::collections::HashSet;
use url::Url;

pub fn normalize(_site: Site, input: &str) -> Result<String> {
    xhomealone::normalize(input)
}
pub fn inputs(site: Site, text: &str) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut values = Vec::new();
    for word in text.split_whitespace() {
        let url = normalize(site, word)?;
        if seen.insert(url.clone()) {
            values.push(url)
        }
    }
    if values.is_empty() {
        bail!("Introduce al menos un enlace.")
    }
    Ok(values)
}
pub fn validate_media(_site: Site, url: &Url, post: &str) -> Result<()> {
    xhomealone::validate_media(url, post)
}
