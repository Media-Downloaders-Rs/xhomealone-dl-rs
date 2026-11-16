//! Ordered mixed-site execution; native engine paths and identities stay intact.
use crate::{
    Cancel, Site,
    engine::{Action, Engine, Event, FileOutcome, RunOptions, Summary},
    settings::Destinations,
    sites,
};
use anyhow::{Result, bail};
use std::{
    collections::HashSet,
    future::Future,
    sync::{Arc, Mutex},
};

pub const SITES: [Site; 1] = [Site::XHomeAlone];
#[derive(Clone, Debug)]
pub struct Link {
    pub site: Site,
    pub url: String,
}
pub fn detect(input: &str) -> Result<Link> {
    let mut matches = SITES.into_iter().filter_map(|site| {
        sites::normalize(site, input)
            .ok()
            .map(|url| Link { site, url })
    });
    let first = matches
        .next()
        .ok_or_else(|| anyhow::anyhow!("Enlace no admitido por los descargadores disponibles"))?;
    if matches.next().is_some() {
        bail!("El enlace corresponde a más de un proveedor")
    }
    Ok(first)
}
pub fn inputs(text: &str) -> Result<Vec<Link>> {
    let mut seen = HashSet::new();
    let mut links = Vec::new();
    for (index, word) in text.split_whitespace().enumerate() {
        let link = detect(word).map_err(|e| anyhow::anyhow!("Enlace {}: {e}", index + 1))?;
        if seen.insert((link.site, link.url.clone())) {
            links.push(link);
        }
    }
    if links.is_empty() {
        bail!("Introduce al menos un enlace")
    }
    Ok(links)
}
pub struct Options {
    pub links: Vec<Link>,
    pub destinations: Destinations,
    pub action: Action,
    pub workers: usize,
    pub password: Option<String>,
}
#[derive(Clone, Debug)]
pub struct ScopedEvent {
    pub site: Site,
    pub event: Event,
}
pub type Sink = Arc<dyn Fn(ScopedEvent) + Send + Sync>;
fn effective_action(_site: Site, action: Action) -> Action {
    action
}
fn add(total: &mut Summary, next: &Summary) {
    total.downloaded += next.downloaded;
    total.skipped += next.skipped;
    total.failed += next.failed;
    total.pending += next.pending;
}
pub async fn run(options: Options, cancel: Cancel, sink: Sink) -> Result<Summary> {
    run_with(options, cancel, sink, |options, cancel, sink| async move {
        Engine::new(options, cancel, sink)?.run().await
    })
    .await
}
pub async fn run_selected(
    options: Options,
    cancel: Cancel,
    sink: Sink,
    selected: Arc<HashSet<(Site, String)>>,
) -> Result<Summary> {
    run_with(options, cancel, sink, move |options, cancel, sink| {
        let paths = Arc::new(
            selected
                .iter()
                .filter(|(site, _)| *site == options.site)
                .map(|(_, path)| path.clone())
                .collect::<HashSet<_>>(),
        );
        async move {
            Engine::new(options, cancel, sink)?
                .run_selected(Some(paths))
                .await
        }
    })
    .await
}
pub async fn run_with<F, Fut>(
    options: Options,
    cancel: Cancel,
    sink: Sink,
    mut execute: F,
) -> Result<Summary>
where
    F: FnMut(RunOptions, Cancel, crate::engine::Sink) -> Fut,
    Fut: Future<Output = Result<Summary>>,
{
    cancel.check()?;
    if options.links.is_empty() || !(1..=16).contains(&options.workers) {
        bail!("Cola o concurrencia no válidas")
    }
    for link in &options.links {
        if sites::normalize(link.site, &link.url)? != link.url {
            bail!("La cola contiene un enlace sin normalizar")
        }
    }
    options
        .destinations
        .validate(&options.links.iter().map(|l| l.site).collect::<Vec<_>>())?;
    let mut total = Summary::default();
    for (index, link) in options.links.iter().enumerate() {
        cancel.check()?;
        let site = link.site;
        sink(ScopedEvent {
            site,
            event: Event::Message(format!(
                "Cola {}/{} · {}",
                index + 1,
                options.links.len(),
                site.key()
            )),
        });
        let partial = Arc::new(Mutex::new(Summary::default()));
        let observed = partial.clone();
        let callback = sink.clone();
        let per_site: crate::engine::Sink = Arc::new(move |event| {
            {
                let mut counts = observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match &event {
                    Event::FileDone {
                        outcome: FileOutcome::Downloaded,
                        ..
                    } => counts.downloaded += 1,
                    Event::FileDone {
                        outcome: FileOutcome::Skipped,
                        ..
                    } => counts.skipped += 1,
                    Event::FileDone {
                        outcome: FileOutcome::Failed(_),
                        ..
                    } => counts.failed += 1,
                    Event::Listing { listing, .. } => counts.pending += listing.pending.len(),
                    _ => {}
                }
            }
            callback(ScopedEvent { site, event });
        });
        let action = effective_action(site, options.action);
        if action != options.action {
            sink(ScopedEvent {
                site,
                event: Event::Message(
                    "Este sitio revisa el catálogo completo al buscar novedades".into(),
                ),
            });
        }
        let job = RunOptions {
            site,
            urls: vec![link.url.clone()],
            root: options.destinations.root(site),
            action,
            workers: options.workers,
            password: None,
        };
        let outcome = execute(job, cancel.clone(), per_site).await;
        cancel.check()?;
        match outcome {
            Ok(summary) => add(&mut total, &summary),
            Err(error) => {
                let mut counts = partial
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                counts.failed += 1;
                add(&mut total, &counts);
                sink(ScopedEvent {
                    site,
                    event: Event::Message(format!("ERROR: {error}")),
                });
            }
        }
    }
    Ok(total)
}
