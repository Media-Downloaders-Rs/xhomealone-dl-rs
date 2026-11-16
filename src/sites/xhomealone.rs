//! XHomeAlone tag catalogs and player declared MP4 sources.
use crate::{Cancel, File, Listing, Pending, safe_name};
use anyhow::{Context, Result, bail};
use scraper::{Html, Selector};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, future::Future};
use url::Url;
const HOST: &str = "xhomealone.com";
fn secure(u: &Url) -> bool {
    u.scheme() == "https"
        && u.host_str() == Some(HOST)
        && u.port().is_none()
        && u.username().is_empty()
        && u.password().is_none()
        && u.fragment().is_none()
}
fn slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 150
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn tag(u: &Url) -> Result<String> {
    if !secure(u) || u.query().is_some() {
        bail!("Introduce una etiqueta HTTPS de XHomeAlone")
    };
    let p: Vec<_> = u.path().trim_matches('/').split('/').collect();
    if p.len() != 2 || p[0] != "tags" || !slug(p[1]) {
        bail!("Etiqueta XHomeAlone no válida")
    };
    Ok(p[1].into())
}
fn post(u: &Url) -> Result<(String, String)> {
    if !secure(u) || u.query().is_some() {
        bail!("Post XHomeAlone no válido")
    };
    let p: Vec<_> = u.path().trim_matches('/').split('/').collect();
    if p.len() != 3
        || p[0] != "videos"
        || p[1].parse::<u64>().ok().filter(|n| *n > 0).is_none()
        || !slug(p[2])
    {
        bail!("Post XHomeAlone no válido")
    };
    Ok((
        p[1].into(),
        format!("https://{HOST}/videos/{}/{}/", p[1], p[2]),
    ))
}
pub fn normalize(input: &str) -> Result<String> {
    let u = Url::parse(input)?;
    Ok(format!("https://{HOST}/tags/{}/", tag(&u)?))
}
pub fn is_post(input: &str) -> bool {
    Url::parse(input).is_ok_and(|u| post(&u).is_ok())
}
pub fn validate_media(u: &Url, p: &str) -> Result<()> {
    let (id, _) = post(&Url::parse(p)?)?;
    if u.host_str().is_some_and(|host| {
        host.strip_prefix('c')
            .and_then(|rest| rest.strip_suffix(".xhomealone.com"))
            .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
    }) && u.scheme() == "https"
        && u.port().is_none()
        && u.username().is_empty()
        && u.password().is_none()
        && u.fragment().is_none()
        && u.path() == "/remote_control.php"
    {
        let pairs: std::collections::HashMap<_, _> = u.query_pairs().collect();
        let expected = format!(
            "/contents/videos/{}/{id}/{id}.mp4",
            id.parse::<u64>()? / 1000 * 1000
        );
        if pairs.len() == 7
            && pairs.get("file").is_some_and(|v| v == &expected)
            && pairs.get("lr").is_some_and(|v| v == "0")
            && pairs.get("time").is_some_and(|v| v.parse::<u64>().is_ok())
            && ["cv", "cv2", "cv3", "cv4"].iter().all(|key| {
                pairs
                    .get(*key)
                    .is_some_and(|v| v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit()))
            })
        {
            return Ok(());
        }
        bail!("La CDN no coincide con el vídeo solicitado")
    }
    let s: Vec<_> = u.path().trim_matches('/').split('/').collect();
    if !secure(u)
        || s.len() != 6
        || s[0] != "get_file"
        || s[1].parse::<u32>().is_err()
        || !(32..=64).contains(&s[2].len())
        || !s[2].bytes().all(|b| b.is_ascii_hexdigit())
        || s[3].parse::<u64>().ok() != Some(id.parse::<u64>()? / 1000 * 1000)
        || s[4] != id
        || s[5] != format!("{id}.mp4")
    {
        bail!("La fuente no coincide con el vídeo solicitado")
    };
    Ok(())
}
fn folder(t: &str) -> String {
    let h = format!("{:x}", Sha256::digest(t.as_bytes()));
    format!("{} [xhomealone-{}]", safe_name(t), &h[..16])
}
pub fn parse_catalog(html: &str, input: &str) -> Result<Vec<(String, String)>> {
    let input = normalize(input)?;
    let owner = tag(&Url::parse(&input)?)?;
    let d = Html::parse_document(html);
    let c = d
        .select(&Selector::parse("link[rel=canonical]").unwrap())
        .next()
        .and_then(|e| e.value().attr("href"))
        .context("Falta canonical de etiqueta")?;
    if normalize(c)? != input {
        bail!("La respuesta corresponde a otra etiqueta")
    };
    let list = d
        .select(&Selector::parse("#list_videos_common_videos_list_items").unwrap())
        .next()
        .context("No se encontró el catálogo")?;
    let base = Url::parse(&input)?;
    let mut out = Vec::new();
    let mut ids = HashSet::new();
    for a in list.select(&Selector::parse("a[href]").unwrap()) {
        let u = base.join(a.value().attr("href").unwrap())?;
        if let Ok((id, url)) = post(&u)
            && ids.insert(id.clone())
        {
            out.push((id, url))
        }
    }
    if out.is_empty() {
        bail!("Etiqueta {owner} sin vídeos o catálogo no reconocido")
    };
    Ok(out)
}
pub fn parse_video(html: &str, post_url: &str, tagname: &str) -> Result<File> {
    let (id, p) = post(&Url::parse(post_url)?)?;
    let re = regex::Regex::new(
        r#"video_url\s*:\s*'function/0/(https://xhomealone\.com/get_file/[^']+)'"#,
    )?;
    let raw = re
        .captures(html)
        .context("No se encontró la fuente MP4 declarada")?
        .get(1)
        .unwrap()
        .as_str();
    let u = Url::parse(raw)?;
    validate_media(&u, &p)?;
    let d = Html::parse_document(html);
    let title = d
        .select(&Selector::parse("h1").unwrap())
        .next()
        .map(|e| e.text().collect::<String>())
        .unwrap_or_else(|| id.clone());
    Ok(File {
        id,
        post: p,
        url: u.to_string(),
        path: format!("{}/{}.mp4", folder(tagname), safe_name(&title)),
        ..Default::default()
    })
}
pub fn browser_source(
    browser: &mut crate::browser::BrowserSession,
    post_url: &str,
    cancel: &Cancel,
) -> Result<String> {
    post(&Url::parse(post_url)?)?;
    browser.navigate(post_url)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        cancel.check()?;
        if let Ok(value) = browser.evaluate("document.querySelector('video')?.currentSrc || ''")
            && let Some(raw) = value.as_str().filter(|value| !value.is_empty())
            && let Ok(url) = Url::parse(raw)
            && validate_media(&url, post_url).is_ok()
        {
            return Ok(url.to_string());
        }
        if std::time::Instant::now() >= deadline {
            bail!("El reproductor no ofreció un enlace MP4 válido")
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
pub async fn resolve<F, Fut>(input: &str, cancel: &Cancel, fetch: &F) -> Result<Listing>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let input = normalize(input)?;
    let t = tag(&Url::parse(&input)?)?;
    let html = fetch(input.clone()).await?;
    cancel.check()?;
    let posts = parse_catalog(&html, &input)?;
    let mut listing = Listing {
        title: folder(&t),
        ..Default::default()
    };
    for (_, url) in posts {
        cancel.check()?;
        match fetch(url.clone())
            .await
            .and_then(|h| parse_video(&h, &url, &t))
        {
            Ok(f) => listing.files.push(f),
            Err(e) => listing.pending.push(Pending {
                url,
                error: e.to_string(),
            }),
        }
    }
    Ok(listing)
}
