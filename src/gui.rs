use crate::{
    Cancel, File, Site,
    engine::{Action, Engine, Event, FileOutcome, RunOptions, Summary},
    global,
    settings::{self, Destinations, Preferences},
    sites,
};
use eframe::egui;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

enum Message {
    Engine(Site, Event),
    Done(Result<Summary, String>),
    Preview((Site, String), Result<egui::ColorImage, String>),
}
struct Row {
    site: Site,
    file: File,
    selection_path: String,
    received: u64,
    total: Option<u64>,
    status: String,
    selected: bool,
}
struct App {
    site: Option<Site>,
    preferences_path: Option<PathBuf>,
    preferences_load_failed: bool,
    overrides: HashMap<Site, String>,
    links: String,
    root: String,
    password: String,
    workers: usize,
    active: bool,
    closing: bool,
    cancel: Cancel,
    sender: mpsc::Sender<Message>,
    receiver: mpsc::Receiver<Message>,
    logs: VecDeque<String>,
    rows: Vec<Row>,
    listed_inputs: Vec<(Site, String)>,
    selection_override: Option<Arc<HashSet<(Site, String)>>>,
    preview_key: Option<(Site, String)>,
    preview_loading: bool,
    preview_image: Option<egui::ColorImage>,
    preview_texture: Option<egui::TextureHandle>,
    preview_error: Option<String>,
    indices: HashMap<(Site, String), usize>,
    status: String,
    diagnostic: Option<Instant>,

    organizing: bool,
}
impl App {
    fn new(site: Option<Site>, diagnostic: bool) -> Self {
        let app_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        Self::from_directory(site, diagnostic, app_dir)
    }
    fn from_directory(site: Option<Site>, diagnostic: bool, app_dir: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel();
        let preferences_path =
            (site.is_none() && !diagnostic).then(|| app_dir.join(".global-dl/settings.json"));
        let mut logs = VecDeque::new();
        let mut preferences_load_failed = false;
        let preferences = if let Some(path) = &preferences_path {
            if path.exists() {
                match settings::load(path) {
                    Ok(p) => p,
                    Err(e) => {
                        preferences_load_failed = true;
                        logs.push_back(format!("No se pudieron leer las preferencias: {e}"));
                        Preferences::default_for(&app_dir)
                    }
                }
            } else {
                Preferences::default_for(&app_dir)
            }
        } else {
            Preferences {
                version: 1,
                workers: 4,
                destinations: Destinations::new(app_dir.join("downloads")),
            }
        };
        Self {
            site,
            root: preferences.destinations.base.display().to_string(),
            overrides: preferences
                .destinations
                .overrides
                .into_iter()
                .map(|(s, p)| (s, p.display().to_string()))
                .collect(),
            preferences_path,
            preferences_load_failed,
            links: String::new(),
            password: String::new(),
            workers: preferences.workers,
            active: false,
            closing: false,
            cancel: Cancel::default(),
            sender,
            receiver,
            logs,
            rows: Vec::new(),
            listed_inputs: Vec::new(),
            selection_override: None,
            preview_key: None,
            preview_loading: false,
            preview_image: None,
            preview_texture: None,
            preview_error: None,
            indices: HashMap::new(),
            status: "Listo".into(),
            diagnostic: diagnostic.then(Instant::now),

            organizing: false,
        }
    }
    fn destinations(&self) -> Destinations {
        Destinations {
            base: PathBuf::from(self.root.trim()),
            overrides: self
                .overrides
                .iter()
                .filter(|(_, p)| !p.trim().is_empty())
                .map(|(s, p)| (*s, PathBuf::from(p.trim())))
                .collect(),
        }
    }
    fn save_preferences(&mut self, explicit: bool) {
        if self.preferences_load_failed && !explicit {
            self.log("Se conserva el archivo de preferencias no leído. Guardar rutas permite reemplazarlo con las rutas mostradas.".into());
            return;
        }
        if let Some(path) = &self.preferences_path {
            let result = settings::save(
                path,
                &Preferences {
                    version: 1,
                    workers: self.workers,
                    destinations: self.destinations(),
                },
            );
            if let Err(e) = result {
                self.log(format!("No se pudieron guardar las rutas: {e}"));
            } else {
                self.preferences_load_failed = false;
            }
        }
    }
    fn log(&mut self, message: String) {
        self.logs.push_back(message);
        while self.logs.len() > 1500 {
            self.logs.pop_front();
        }
    }

    fn start(&mut self, action: Action, ctx: &egui::Context) {
        let links = if let Some(site) = self.site {
            sites::inputs(site, &self.links).map(|urls| {
                urls.into_iter()
                    .map(|url| global::Link { site, url })
                    .collect()
            })
        } else {
            global::inputs(&self.links)
        };
        let links = match links {
            Ok(l) => l,
            Err(e) => {
                self.log(e.to_string());
                return;
            }
        };
        if self.root.trim().is_empty() {
            self.log("Elige una carpeta de destino".into());
            return;
        }
        let input_keys: Vec<_> = links
            .iter()
            .map(|link| (link.site, link.url.clone()))
            .collect();
        let selection = selection_for(action, &self.rows, &self.listed_inputs, &input_keys);
        if selection.as_ref().is_some_and(HashSet::is_empty) {
            self.log("Selecciona al menos un archivo".into());
            return;
        }
        let selection = selection.map(Arc::new);
        let destinations = self.destinations();
        if self.site.is_none() {
            if let Err(e) = destinations.validate(&links.iter().map(|l| l.site).collect::<Vec<_>>())
            {
                self.log(e.to_string());
                return;
            }
            self.save_preferences(false);
        }
        self.rows.clear();
        self.indices.clear();
        self.listed_inputs = input_keys;
        self.selection_override = selection.clone();
        self.active = true;
        self.cancel = Cancel::default();
        self.status = "Trabajando...".into();
        let mode = self.site;
        let root = PathBuf::from(self.root.trim());
        let workers = self.workers.clamp(1, 16);
        let password = (!self.password.is_empty()).then(|| self.password.clone());
        let cancel = self.cancel.clone();
        let sender = self.sender.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let events = sender.clone();
            let redraw = repaint.clone();
            let sink: global::Sink = Arc::new(move |e| {
                let _ = events.send(Message::Engine(e.site, e.event));
                redraw.request_repaint();
            });
            let result = (|| {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(4)
                    .enable_all()
                    .build()?;
                runtime.block_on(async {
                    if let Some(site) = mode {
                        let engine_sink: crate::engine::Sink =
                            Arc::new(move |event| sink(global::ScopedEvent { site, event }));
                        let selected_paths = selection.as_ref().map(|items| {
                            Arc::new(
                                items
                                    .iter()
                                    .filter(|(item_site, _)| *item_site == site)
                                    .map(|(_, path)| path.clone())
                                    .collect::<HashSet<_>>(),
                            )
                        });
                        Engine::new(
                            RunOptions {
                                site,
                                urls: links.into_iter().map(|l| l.url).collect(),
                                root,
                                action,
                                workers,
                                password,
                            },
                            cancel,
                            engine_sink,
                        )?
                        .run_selected(selected_paths)
                        .await
                    } else {
                        let options = global::Options {
                            links,
                            destinations,
                            action,
                            workers,
                            password,
                        };
                        if let Some(selected) = selection {
                            global::run_selected(options, cancel, sink, selected).await
                        } else {
                            global::run(options, cancel, sink).await
                        }
                    }
                })
            })();
            let _ = sender.send(Message::Done(
                result.map_err(|e: anyhow::Error| e.to_string()),
            ));
            repaint.request_repaint();
        });
    }
    fn request_preview(&mut self, site: Site, file: File, ctx: &egui::Context) {
        let Some(url) = preview_url(&file) else {
            return;
        };
        let key = (site, file.path.clone());
        self.preview_key = Some(key.clone());
        self.preview_loading = true;
        self.preview_image = None;
        self.preview_texture = None;
        self.preview_error = None;
        let sender = self.sender.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<egui::ColorImage> {
                use std::io::Read;
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(20))
                    .user_agent(file.preview_user_agent.as_deref().unwrap_or("Mozilla/5.0"))
                    .build()?;
                let image_accept = "image/avif,image/webp,image/apng,image/*,*/*;q=0.8";
                let send = |origin: bool| {
                    let mut request = client
                        .get(&url)
                        .header(reqwest::header::ACCEPT, image_accept);
                    if origin {
                        request = request.header(reqwest::header::ORIGIN, "https://xhomealone.com");
                    }
                    request.send()
                };
                let mut response = send(false).map_err(|e| {
                    if e.is_timeout() {
                        anyhow::anyhow!("Tiempo de espera agotado al cargar la miniatura")
                    } else {
                        anyhow::anyhow!("Error de red al cargar la miniatura")
                    }
                })?;
                if site == Site::XHomeAlone && matches!(response.status().as_u16(), 401 | 403) {
                    response = send(true).map_err(|e| {
                        if e.is_timeout() {
                            anyhow::anyhow!("Tiempo de espera agotado al cargar la miniatura")
                        } else {
                            anyhow::anyhow!("Error de red al cargar la miniatura")
                        }
                    })?;
                }
                if !response.status().is_success() {
                    let status = response.status();
                    let expired = url::Url::parse(&url)
                        .ok()
                        .and_then(|u| {
                            u.query_pairs()
                                .find(|(key, _)| key == "expires")
                                .and_then(|(_, value)| value.parse::<u64>().ok())
                        })
                        .is_some_and(|expiry| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .is_ok_and(|now| expiry <= now.as_secs())
                        });
                    if expired {
                        anyhow::bail!("La URL de la miniatura caducó; pulsa Listar de nuevo")
                    }
                    anyhow::bail!("El servidor rechazó la miniatura (HTTP {status})")
                }
                let mime = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("");
                if !mime.starts_with("image/") {
                    anyhow::bail!("El servidor devolvió contenido distinto de una imagen")
                }
                if response
                    .content_length()
                    .is_some_and(|size| size > 6_000_000)
                {
                    anyhow::bail!("La miniatura es demasiado grande")
                }
                let mut bytes = Vec::new();
                response.take(6_000_001).read_to_end(&mut bytes)?;
                if bytes.len() > 6_000_000 {
                    anyhow::bail!("La miniatura es demasiado grande")
                }
                let decoded = image::load_from_memory(&bytes)
                    .map_err(|_| anyhow::anyhow!("Formato de miniatura no compatible"))?
                    .thumbnail(520, 300)
                    .to_rgba8();
                Ok(egui::ColorImage::from_rgba_unmultiplied(
                    [decoded.width() as usize, decoded.height() as usize],
                    decoded.as_raw(),
                ))
            })()
            .map_err(|error: anyhow::Error| error.to_string());
            let _ = sender.send(Message::Preview(key, result));
            repaint.request_repaint();
        });
    }
    fn messages(&mut self) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                Message::Engine(site, Event::Message(s)) => {
                    self.log(format!("[{}] {s}", site.key()))
                }
                Message::Engine(site, Event::Listing { listing, .. }) => {
                    self.log(format!(
                        "{}: {} archivos; {} pendientes",
                        listing.title,
                        listing.files.len(),
                        listing.pending.len()
                    ));
                    let mut files = listing.files;
                    files.sort_by(|a, b| b.published_at.cmp(&a.published_at));
                    for file in files {
                        if self.indices.contains_key(&(site, file.path.clone())) {
                            continue;
                        }
                        self.indices
                            .insert((site, file.path.clone()), self.rows.len());
                        let selected = self
                            .selection_override
                            .as_ref()
                            .is_none_or(|items| items.contains(&(site, file.path.clone())));
                        self.rows.push(Row {
                            site,
                            total: file.expected_size,
                            selection_path: file.path.clone(),
                            file,
                            received: 0,
                            status: "Pendiente".into(),
                            selected,
                        });
                    }
                    for pending in listing.pending {
                        self.log(format!("Pendiente: {} — {}", pending.url, pending.error));
                    }
                }
                Message::Engine(
                    site,
                    Event::Progress {
                        path,
                        received,
                        total,
                    },
                ) => {
                    if let Some(index) = self.indices.get(&(site, path.clone())) {
                        let row = &mut self.rows[*index];
                        row.received = received;
                        row.total = total;
                        row.status = "Descargando".into();
                    }
                }
                Message::Engine(site, Event::FileDone { path, outcome }) => {
                    let status = match outcome {
                        FileOutcome::Downloaded => "Descargado".into(),
                        FileOutcome::Skipped => "Omitido".into(),
                        FileOutcome::Failed(s) => format!("Error: {s}"),
                        FileOutcome::Cancelled => "Cancelado".into(),
                    };
                    if let Some(index) = self.indices.get(&(site, path.clone())) {
                        self.rows[*index].status = status.clone();
                    }
                    self.log(format!("{status}: {path}"));
                }
                Message::Engine(
                    site,
                    Event::SourceChanged {
                        previous_path,
                        file,
                    },
                ) => {
                    if let Some(index) = self.indices.remove(&(site, previous_path)) {
                        self.indices.insert((site, file.path.clone()), index);
                        let row = &mut self.rows[index];
                        row.total = file.expected_size;
                        row.file = file;
                        row.received = 0;
                        row.status = "Enlace renovado".into();
                    }
                }
                Message::Engine(_, Event::Finished(_)) => {}
                Message::Preview(key, result) => {
                    if self.preview_key.as_ref() == Some(&key) {
                        self.preview_loading = false;
                        match result {
                            Ok(image) => self.preview_image = Some(image),
                            Err(error) => self.preview_error = Some(error),
                        }
                    }
                }
                Message::Done(result) => {
                    self.active = false;
                    self.status = match result {
                        Ok(s) if self.organizing => format!(
                            "Organización: {} reubicados · {} archivados · {} pendientes",
                            s.downloaded, s.skipped, s.failed
                        ),
                        Ok(s) => format!(
                            "Final: {} descargados · {} omitidos · {} fallidos · {} pendientes",
                            s.downloaded, s.skipped, s.failed, s.pending
                        ),
                        Err(e) => e,
                    };
                    self.organizing = false;
                    self.log(self.status.clone());
                }
            }
        }
    }
}
impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.messages();
        if let Some(image) = self.preview_image.take() {
            self.preview_texture =
                Some(ctx.load_texture("preview", image, egui::TextureOptions::LINEAR));
        }

        if self
            .diagnostic
            .is_some_and(|t| t.elapsed() > Duration::from_millis(400))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.active {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.closing = true;
            self.cancel.cancel();
            self.status = "Cancelando antes de cerrar…".into();
        }
        if self.closing && !self.active {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(
                self.site
                    .map(|s| s.title())
                    .unwrap_or_else(|| "Global DL - Rust".into()),
            );
            ui.add_space(8.0);
            ui.add_enabled_ui(!self.active, |ui| {
                ui.label("Enlaces, uno por línea");
                ui.add(
                    egui::TextEdit::multiline(&mut self.links)
                        .desired_rows(4)
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    ui.label(if self.site.is_none() {
                        "Destino propio"
                    } else {
                        "Destino"
                    });
                    ui.add(
                        egui::TextEdit::singleline(&mut self.root)
                            .desired_width((ui.available_width() - 85.0).max(150.0)),
                    );
                    if ui.button("Elegir…").clicked()
                        && let Some(folder) = rfd::FileDialog::new().pick_folder()
                    {
                        self.root = folder.display().to_string();
                    }
                });
                if self.site.is_none() {
                    ui.label("Las rutas por sitio tienen prioridad. Cambiarlas no mueve archivos.");
                    ui.collapsing("Rutas por sitio (vacio usa el destino propio)", |ui| {
                        for site in global::SITES {
                            ui.horizontal(|ui| {
                                ui.label(site.key());
                                let path = self.overrides.entry(site).or_default();
                                ui.add(
                                    egui::TextEdit::singleline(path)
                                        .hint_text(
                                            PathBuf::from(self.root.trim())
                                                .join(site.key())
                                                .display()
                                                .to_string(),
                                        )
                                        .desired_width((ui.available_width() - 85.0).max(120.0)),
                                );
                                if ui.button("Elegir").clicked()
                                    && let Some(folder) = rfd::FileDialog::new().pick_folder()
                                {
                                    *path = folder.display().to_string();
                                }
                            });
                        }
                        if ui.button("Guardar rutas").clicked() {
                            self.save_preferences(true);
                        }
                        if ui.button("Usar destino propio en todos").clicked() {
                            self.overrides.clear();
                        }
                        if ui.button("Detectar carpetas existentes").clicked()
                            && let Some(app_dir) = self
                                .preferences_path
                                .as_ref()
                                .and_then(|p| p.parent())
                                .and_then(|p| p.parent())
                        {
                            self.overrides = Preferences::default_for(app_dir)
                                .destinations
                                .overrides
                                .into_iter()
                                .map(|(site, path)| (site, path.display().to_string()))
                                .collect();
                        }
                    });
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label("Simultáneas:");
                ui.add_enabled(
                    !self.active,
                    egui::DragValue::new(&mut self.workers).range(1..=16),
                );
                let actions = vec![("Listar", Action::List), ("Descargar", Action::Download)];

                for (label, action) in actions {
                    if ui
                        .add_enabled(!self.active, egui::Button::new(label))
                        .clicked()
                    {
                        self.start(action, ctx);
                    }
                }
                if ui
                    .add_enabled(self.active, egui::Button::new("Cancelar"))
                    .clicked()
                {
                    self.cancel.cancel();
                    self.status = "Cancelando…".into();
                }
            });
            ui.label(&self.status);

            ui.separator();
            if self.rows.is_empty() && !self.active {
                ui.small("Pulsa Listar para elegir archivos; Descargar sin lista descarga todos.");
            }
            if !self.rows.is_empty() {
                let selected = self.rows.iter().filter(|row| row.selected).count();
                ui.horizontal(|ui| {
                    ui.label(format!("Seleccionados: {selected}/{}", self.rows.len()));
                    if ui
                        .add_enabled(!self.active, egui::Button::new("Todos"))
                        .clicked()
                    {
                        for row in &mut self.rows {
                            row.selected = true;
                        }
                    }
                    if ui
                        .add_enabled(!self.active, egui::Button::new("Ninguno"))
                        .clicked()
                    {
                        for row in &mut self.rows {
                            row.selected = false;
                        }
                    }
                });
            }
            let height = (ui.available_height() * 0.60).max(100.0);
            let mut requested_preview = None;
            let show_site = self.site.is_none();
            egui::ScrollArea::vertical()
                .id_salt("files")
                .max_height(height)
                .show_rows(ui, 24.0, self.rows.len(), |ui, range| {
                    for index in range {
                        let row = &mut self.rows[index];
                        ui.horizontal(|ui| {
                            ui.add_enabled(
                                !self.active,
                                egui::Checkbox::without_text(&mut row.selected),
                            );
                            ui.label(format!("{}", index + 1));
                            if show_site {
                                ui.label(row.site.key());
                            }

                            let has_preview = preview_url(&row.file).is_some();
                            if has_preview && ui.button("Vista").clicked() {
                                requested_preview = Some((row.site, row.file.clone()));
                            }
                            ui.add_sized(
                                [
                                    ui.available_width() * if has_preview { 0.42 } else { 0.52 },
                                    20.0,
                                ],
                                egui::Label::new(&row.file.path).truncate(),
                            );
                            if let Some(total) = row.total.filter(|n| *n > 0) {
                                ui.add(
                                    egui::ProgressBar::new(
                                        (row.received as f64 / total as f64) as f32,
                                    )
                                    .desired_width(100.0)
                                    .show_percentage(),
                                );
                            }
                            ui.label(&row.status);
                        });
                    }
                });
            if let Some((site, file)) = requested_preview {
                self.request_preview(site, file, ctx);
            }
            ui.separator();
            ui.label("Registro");
            egui::ScrollArea::vertical()
                .id_salt("log")
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.logs {
                        ui.monospace(line);
                    }
                });
        });
        if let Some((_, path)) = self.preview_key.as_ref() {
            let mut open = true;
            egui::Window::new("Miniatura")
                .open(&mut open)
                .show(ctx, |ui| {
                    ui.label(path);
                    if self.preview_loading {
                        ui.spinner();
                        ui.label("Cargando miniatura...");
                    }
                    if let Some(texture) = &self.preview_texture {
                        let size = texture.size_vec2();
                        let scale = (520.0 / size.x).min(300.0 / size.y).min(1.0);
                        ui.image((texture.id(), size * scale));
                    }
                    if let Some(error) = &self.preview_error {
                        ui.label(error);
                    }
                });
            if !open {
                self.preview_key = None;
                self.preview_texture = None;
            }
        }
        if self.active || self.diagnostic.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}
fn selection_for(
    action: Action,
    rows: &[Row],
    listed_inputs: &[(Site, String)],
    input_keys: &[(Site, String)],
) -> Option<HashSet<(Site, String)>> {
    if action != Action::Download || rows.is_empty() || listed_inputs != input_keys {
        return None;
    }
    Some(
        rows.iter()
            .filter(|row| row.selected)
            .map(|row| (row.site, row.selection_path.clone()))
            .collect(),
    )
}
fn preview_url(file: &File) -> Option<String> {
    if let Some(url) = &file.thumbnail {
        return url.starts_with("https://").then(|| url.clone());
    }
    let path = file
        .url
        .split('?')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    (file.url.starts_with("https://")
        && [".jpg", ".jpeg", ".png", ".webp"]
            .iter()
            .any(|ext| path.ends_with(ext)))
    .then(|| file.url.clone())
}
pub fn run(site: Site) -> eframe::Result<()> {
    launch(Some(site))
}
pub fn run_global() -> eframe::Result<()> {
    launch(None)
}
fn launch(site: Option<Site>) -> eframe::Result<()> {
    let diagnostic = std::env::args().any(|s| s == "--smoke-gui");
    let title = site
        .map(|s| s.title())
        .unwrap_or_else(|| "Global DL - Rust".into());
    eframe::run_native(
        &title,
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size(if site.is_none() {
                    [1060.0, 780.0]
                } else {
                    [980.0, 720.0]
                })
                .with_min_inner_size([780.0, 520.0]),
            ..Default::default()
        },
        Box::new(move |_| Ok(Box::new(App::new(site, diagnostic)))),
    )
}

#[cfg(test)]
mod global_gui_tests {
    use super::*;
    fn listing() -> Event {
        Event::Listing {
            url: "fixture".into(),
            listing: crate::Listing {
                title: "same".into(),
                files: vec![File {
                    path: "same/file.jpg".into(),
                    ..Default::default()
                }],
                pending: Vec::new(),
            },
        }
    }
    #[test]
    fn chosen_files_apply_only_to_the_listed_inputs() {
        let mut app = App::new(Some(Site::XHomeAlone), true);
        let key = vec![(
            Site::XHomeAlone,
            "https://xhomealone.com/tags/fixture-performer-000/".into(),
        )];
        app.listed_inputs = key.clone();
        app.sender
            .send(Message::Engine(Site::XHomeAlone, listing()))
            .unwrap();
        app.messages();
        assert_eq!(
            selection_for(Action::Download, &app.rows, &app.listed_inputs, &key)
                .unwrap()
                .len(),
            1
        );
        app.rows[0].file.path = "same/file.mp4".into();
        assert!(
            selection_for(Action::Download, &app.rows, &app.listed_inputs, &key)
                .unwrap()
                .contains(&(Site::XHomeAlone, "same/file.jpg".into()))
        );
        app.rows[0].selected = false;
        assert!(
            selection_for(Action::Download, &app.rows, &app.listed_inputs, &key)
                .unwrap()
                .is_empty()
        );
        assert!(
            selection_for(
                Action::Download,
                &app.rows,
                &app.listed_inputs,
                &[(
                    Site::XHomeAlone,
                    "https://xhomealone.com/tags/other/".into()
                )]
            )
            .is_none()
        );
    }
    #[test]
    fn preview_only_uses_explicit_or_image_sources() {
        let mut file = File {
            url: "https://example.test/video.mp4".into(),
            ..Default::default()
        };
        assert!(preview_url(&file).is_none());
        file.thumbnail = Some("https://example.test/thumb.jpg".into());
        assert_eq!(preview_url(&file).as_deref(), file.thumbnail.as_deref());
        file.thumbnail = None;
        file.url = "https://example.test/image.jpg?signature=x".into();
        assert!(preview_url(&file).is_some());
    }
}
