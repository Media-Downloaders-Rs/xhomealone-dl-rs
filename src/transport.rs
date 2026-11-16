use crate::{Cancel, Log, Site, sites};
use anyhow::{Result, bail};
use reqwest::{Client, Response, header};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use url::Url;

pub trait UrlPolicy: Send + Sync {
    fn check(&self, url: &Url, post: &str) -> Result<()>;
    fn check_redirect(&self, url: &Url, post: &str, _original: &Url) -> Result<()> {
        self.check(url, post)
    }
}

pub struct SitePolicy(pub Site);
impl UrlPolicy for SitePolicy {
    fn check(&self, url: &Url, post: &str) -> Result<()> {
        if url.as_str() == post
            && (sites::xhomealone::normalize(post).is_ok() || sites::xhomealone::is_post(post))
        {
            return Ok(());
        }
        sites::xhomealone::validate_media(url, post)
    }
}
#[derive(Clone)]
pub struct RetryOptions {
    pub attempts: usize,
    pub base: Duration,
}
impl Default for RetryOptions {
    fn default() -> Self {
        Self {
            attempts: 4,
            base: Duration::from_secs(5),
        }
    }
}
#[derive(Debug)]
pub struct Transient {
    pub message: String,
    pub after: Option<Duration>,
}
impl std::fmt::Display for Transient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}
impl std::error::Error for Transient {}
#[derive(Debug)]
pub struct HttpFailure(pub reqwest::StatusCode);
impl std::fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.0)
    }
}
impl std::error::Error for HttpFailure {}
pub fn network_error(error: reqwest::Error) -> anyhow::Error {
    // Reqwest's Display omits the nested cause (often only "error sending
    // request"). Report its typed category and host, never URLs or credentials.
    let host = error.url().and_then(Url::host_str).unwrap_or("el servidor");
    let message = if error.is_timeout() {
        if error.is_connect() {
            format!("Tiempo de espera agotado al conectar con {host}")
        } else {
            format!("Tiempo de espera agotado al recibir datos de {host}")
        }
    } else if error.is_connect() {
        format!("No se pudo conectar con {host} (fallo de conexión, DNS o TLS)")
    } else if error.is_body() {
        format!("Se interrumpió la recepción de datos de {host}")
    } else {
        format!("Falló la petición de red a {host}")
    };
    Transient {
        message,
        after: None,
    }
    .into()
}
pub struct Transport {
    pub cancel: Cancel,
    pub policy: Arc<dyn UrlPolicy>,
    pub retry_options: RetryOptions,
    pub log: Log,
    client: Client,
    pause: Mutex<Instant>,
    next_page: Mutex<Instant>,
    browser_session: Mutex<Option<(String, header::HeaderValue, header::HeaderValue)>>,
}
impl Transport {
    pub fn new(
        policy: Arc<dyn UrlPolicy>,
        cancel: Cancel,
        log: Log,
        retry_options: RetryOptions,
    ) -> Result<Self> {
        let client = Client::builder()
            .user_agent("Mozilla/5.0")
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            cancel,
            policy,
            log,
            retry_options,
            client,
            pause: Mutex::new(Instant::now()),
            next_page: Mutex::new(Instant::now()),
            browser_session: Mutex::new(None),
        })
    }
    pub fn set_browser_session(&self, origin: &str, cookie: &str, user_agent: &str) -> Result<()> {
        let url = Url::parse(origin).map_err(|_| anyhow::anyhow!("Origen de sesión no válido"))?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/"
            || cookie.len() > 65536
            || user_agent.is_empty()
            || user_agent.len() > 4096
        {
            bail!("Sesión de navegador no válida")
        }
        let mut cookie = header::HeaderValue::from_str(cookie)
            .map_err(|_| anyhow::anyhow!("Cabecera de sesión no válida"))?;
        let mut agent = header::HeaderValue::from_str(user_agent)
            .map_err(|_| anyhow::anyhow!("Cabecera de sesión no válida"))?;
        cookie.set_sensitive(true);
        agent.set_sensitive(true);
        *self
            .browser_session
            .lock()
            .map_err(|_| anyhow::anyhow!("Sesión de navegador bloqueada"))? =
            Some((url.origin().ascii_serialization(), cookie, agent));
        Ok(())
    }
    fn browser_headers(
        &self,
        url: &Url,
    ) -> Result<Option<(header::HeaderValue, header::HeaderValue)>> {
        if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
            return Ok(None);
        }
        let session = self
            .browser_session
            .lock()
            .map_err(|_| anyhow::anyhow!("Sesión de navegador bloqueada"))?;
        Ok(session
            .as_ref()
            .filter(|(origin, _, _)| *origin == url.origin().ascii_serialization())
            .map(|(_, cookie, agent)| (cookie.clone(), agent.clone())))
    }

    pub async fn page_turn(&self, interval: Duration) -> Result<()> {
        loop {
            self.ready().await?;
            let wait = {
                let mut next = self
                    .next_page
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Pausa de páginas bloqueada"))?;
                let now = Instant::now();
                if *next <= now {
                    *next = now + interval;
                    return Ok(());
                }
                *next - now
            };
            self.cancel.sleep(wait).await?;
        }
    }

    async fn ready(&self) -> Result<()> {
        loop {
            self.cancel.check()?;
            let until = *self
                .pause
                .lock()
                .map_err(|_| anyhow::anyhow!("Pausa de red inválida"))?;
            let now = Instant::now();
            if until <= now {
                return Ok(());
            }
            self.cancel.sleep(until - now).await?;
        }
    }
    pub async fn request(
        &self,
        input: &str,
        post: &str,
        range: Option<(u64, String)>,
    ) -> Result<Response> {
        self.request_with_accept(input, post, range, None, false)
            .await
    }
    pub async fn head(&self, url: &str, post: &str) -> Result<Response> {
        self.retry_limited(1, || self.request_with_accept(url, post, None, None, true))
            .await
    }
    async fn request_with_accept(
        &self,
        input: &str,
        post: &str,
        mut range: Option<(u64, String)>,
        accept: Option<&str>,
        head: bool,
    ) -> Result<Response> {
        let mut url = Url::parse(input)?;
        let original = url.clone();
        for step in 0..10 {
            if step == 0 {
                self.policy.check(&url, post)?;
            } else {
                self.policy.check_redirect(&url, post, &original)?;
            }
            self.ready().await?;

            let mut request = self
                .client
                .request(
                    if head {
                        reqwest::Method::HEAD
                    } else {
                        reqwest::Method::GET
                    },
                    url.clone(),
                )
                .header(header::REFERER, post)
                .header(header::ACCEPT_ENCODING, "identity");

            if let Some(accept) = accept {
                request = request.header(header::ACCEPT, accept);
                self.page_turn(Duration::from_millis(200)).await?;
            }
            if let Some((cookie, agent)) = self.browser_headers(&url)? {
                request = request.header(header::USER_AGENT, agent);
                if !cookie.is_empty() {
                    request = request.header(header::COOKIE, cookie);
                }
            }
            if let Some((start, validator)) = &range {
                request = request
                    .header(header::RANGE, format!("bytes={start}-"))
                    .header(header::IF_RANGE, validator);
            }
            let response = self
                .cancel
                .race(request.send())
                .await?
                .map_err(network_error)?;
            let status = response.status();
            if status.is_redirection() {
                let next = response
                    .headers()
                    .get(header::LOCATION)
                    .ok_or_else(|| anyhow::anyhow!("Redirección sin destino"))?
                    .to_str()?;
                url = url.join(next)?;
                // Validators belong to a representation, not to a redirect chain.
                // Restart at every redirect rather than forward a stale range.
                range = None;
                continue;
            }
            if matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504) {
                let after = response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| {
                        s.parse::<u64>().ok().map(Duration::from_secs).or_else(|| {
                            httpdate::parse_http_date(s).ok().map(|d| {
                                d.duration_since(std::time::SystemTime::now())
                                    .unwrap_or_default()
                            })
                        })
                    });
                return Err(Transient {
                    message: format!("HTTP {status}"),
                    after,
                }
                .into());
            }
            if !status.is_success() {
                return Err(HttpFailure(status).into());
            }
            return Ok(response);
        }
        bail!("Demasiadas redirecciones")
    }
    pub async fn retry<T, F, Fut>(&self, action: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.retry_limited(4, action).await
    }
    /// Prefer a confirmed alternate representation after a bounded attempt,
    /// while retaining shared Retry-After pacing for rate-limited servers.
    pub async fn retry_limited<T, F, Fut>(&self, limit: usize, mut action: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let attempts = self
            .retry_options
            .attempts
            .clamp(1, 4)
            .min(limit.clamp(1, 4));
        for attempt in 0..attempts {
            self.ready().await?;
            match action().await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    self.cancel.check()?;
                    let Some(transient) = error.downcast_ref::<Transient>() else {
                        return Err(error);
                    };
                    let delay = (self.retry_options.base * 2u32.pow(attempt as u32))
                        .max(transient.after.unwrap_or_default());
                    {
                        let mut pause = self
                            .pause
                            .lock()
                            .map_err(|_| anyhow::anyhow!("Pausa de red inválida"))?;
                        *pause = (*pause).max(Instant::now() + delay);
                    }
                    (self.log)(format!(
                        "{}; pausa compartida {:.1} s ({}/{})",
                        transient.message,
                        delay.as_secs_f64(),
                        attempt + 1,
                        attempts
                    ));
                    if attempt + 1 == attempts {
                        return Err(error);
                    }
                }
            }
        }
        unreachable!()
    }
    pub async fn get(&self, url: &str, post: &str) -> Result<Response> {
        self.retry(|| self.request(url, post, None)).await
    }

    pub async fn html(&self, url: &str) -> Result<String> {
        self.retry(|| async {
            let mut response = self.request(url, url, None).await?;
            if !response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .contains("text/html")
            {
                bail!("La página no es HTML")
            }
            let mut data = Vec::new();
            while let Some(chunk) = self
                .cancel
                .race(response.chunk())
                .await?
                .map_err(network_error)?
            {
                if data.len() + chunk.len() > 8 * 1024 * 1024 {
                    bail!("Página HTML demasiado grande")
                }
                data.extend_from_slice(&chunk);
            }
            Ok(String::from_utf8_lossy(&data).into_owned())
        })
        .await
    }
    pub async fn json(&self, url: &str, post: &str) -> Result<serde_json::Value> {
        self.retry(|| async {
            let mut response = self.request(url, post, None).await?;
            let mime = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .split(';')
                .next()
                .unwrap_or("")
                .trim();
            if !mime.eq_ignore_ascii_case("application/json") {
                bail!("La API no devolvió JSON")
            }
            let mut data = Vec::new();
            while let Some(chunk) = self
                .cancel
                .race(response.chunk())
                .await?
                .map_err(network_error)?
            {
                if data.len() + chunk.len() > 4 * 1024 * 1024 {
                    bail!("La respuesta JSON supera el límite")
                }
                data.extend_from_slice(&chunk);
            }

            serde_json::from_slice(&data)
                .map_err(|_| anyhow::anyhow!("La API devolvió JSON no reconocido"))
        })
        .await
    }
}
