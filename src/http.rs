use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{
    ACCEPT_RANGES, ALLOW, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_TYPE, HeaderValue, RANGE,
};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder as ConnectionBuilder;
use thiserror::Error;
use tokio::io::AsyncSeekExt;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast};
use tokio_util::io::ReaderStream;

use crate::install_page;
use crate::model::format_bytes;
use crate::service::{ServiceError, ShareService};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type ResponseBody = http_body_util::combinators::BoxBody<Bytes, BoxError>;

/// Upper bound on simultaneously served connections. Without one, a handful of
/// idle sockets can exhaust the process and stall a real OTA download.
const MAX_CONNECTIONS: usize = 256;
/// How long a client may take to send its request head before being dropped.
/// Bodies and responses are deliberately not capped: an IPA download over a
/// slow phone connection is legitimately long-lived.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How long shutdown waits for in-flight requests. A phone part-way through a
/// 200 MB IPA download should not have the socket pulled out from under it
/// because someone pressed Ctrl-C.
const GRACEFUL_DRAIN_TIMEOUT: Duration = Duration::from_secs(120);
/// Minimum time between two progress lines for one download: frequent enough
/// that a stalled transfer is obvious, rare enough that a fast network does
/// not turn a 200 MB download into a line-per-chunk scroll.
const PROGRESS_REPORT_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct HttpState {
    pub service: Arc<ShareService>,
}

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("request I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("service failed: {0}")]
    Service(#[from] ServiceError),
    #[error("requested range is not satisfiable")]
    RangeNotSatisfiable,
}

/// The origin serves only the four public OTA resources needed by an iPhone.
/// Artifact management, uploads, device installation, and health APIs belong
/// to the removed long-running `serve` product and are intentionally absent.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resource {
    Manifest(String),
    Download(String),
    Icon(String),
    InstallPage(String),
}

/// GET and HEAD both resolve to the matched resource; `HttpState::handle`
/// strips the body for HEAD once the response is built, so the two methods
/// share every other code path. Any other method on a known path is a 405,
/// not a 404, so a client can tell "wrong verb" from "wrong resource".
#[derive(Debug, Clone, PartialEq, Eq)]
enum Route {
    Found(Resource),
    MethodNotAllowed,
    NotFound,
}

fn route(method: &Method, path: &str) -> Route {
    let segments = path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let resource = match segments.as_slice() {
        ["api", "v1", "artifacts", id, "manifest.plist"] => {
            Some(Resource::Manifest((*id).to_string()))
        }
        ["api", "v1", "artifacts", id, "download.ipa"] => {
            Some(Resource::Download((*id).to_string()))
        }
        ["api", "v1", "artifacts", id, "icon.png"] => Some(Resource::Icon((*id).to_string())),
        ["install", id] => Some(Resource::InstallPage((*id).to_string())),
        _ => None,
    };
    match resource {
        None => Route::NotFound,
        Some(resource) => match *method {
            Method::GET | Method::HEAD => Route::Found(resource),
            _ => Route::MethodNotAllowed,
        },
    }
}

impl HttpState {
    pub async fn handle(
        &self,
        request: Request<Incoming>,
    ) -> Result<Response<ResponseBody>, Infallible> {
        // Decided once, up front: whichever path produces the response below
        // (success or error), a HEAD request must end up with an empty body.
        let is_head = request.method() == Method::HEAD;
        let route = route(request.method(), request.uri().path());
        let response = match self.dispatch(route, is_head, request).await {
            Ok(response) => response,
            Err(error) => error_response(error),
        };
        Ok(if is_head {
            empty_body(response)
        } else {
            response
        })
    }

    async fn dispatch(
        &self,
        route: Route,
        is_head: bool,
        request: Request<Incoming>,
    ) -> Result<Response<ResponseBody>, HttpError> {
        match route {
            Route::Found(Resource::Manifest(id)) => {
                let artifact = self.service.servable_artifact(&id)?;
                let mut response = text_response(
                    StatusCode::OK,
                    "application/xml; charset=utf-8",
                    self.service.manifest(artifact).await?,
                );
                no_store(&mut response);
                Ok(response)
            }
            Route::Found(Resource::Download(id)) => self.download(&id, is_head, request).await,
            Route::Found(Resource::Icon(id)) => self.icon_download(&id).await,
            Route::Found(Resource::InstallPage(id)) => self.install_page(&id).await,
            Route::MethodNotAllowed => Ok(method_not_allowed_response()),
            Route::NotFound => Ok(json_response(
                StatusCode::NOT_FOUND,
                serde_json::json!({"error":"not found"}),
            )),
        }
    }

    async fn download(
        &self,
        artifact_id: &str,
        is_head: bool,
        request: Request<Incoming>,
    ) -> Result<Response<ResponseBody>, HttpError> {
        if is_head {
            // A HEAD must report the same headers a GET would without
            // spending one of the user's --max-downloads attempts, so it
            // validates through `servable_artifact` rather than
            // `authorize_download`, which atomically claims a slot.
            // `Range` is not honoured on HEAD; the full entity length is
            // always what gets reported.
            let artifact = self.service.servable_artifact(artifact_id)?;
            let path = self.service.download_path(artifact_id)?.to_path_buf();
            // `HttpState::handle` discards this body via `empty_body` right
            // after it is built, so a reporter attached here would see every
            // HEAD request as an interrupted download.
            return file_response(path, &artifact.file_name, None, false).await;
        }
        let grant_token = query_param(request.uri().query(), "download");
        let artifact = self
            .service
            .authorize_download(artifact_id, grant_token.as_deref())
            .await?;
        let path = self.service.download_path(artifact_id)?.to_path_buf();
        let range = request
            .headers()
            .get(RANGE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        tracing::info!(
            artifact_id = %artifact.id,
            file_name = %artifact.file_name,
            range = ?range,
            "IPA download started"
        );
        file_response(path, &artifact.file_name, range.as_deref(), true).await
    }

    async fn icon_download(&self, artifact_id: &str) -> Result<Response<ResponseBody>, HttpError> {
        self.service.viewable_artifact(artifact_id)?;
        let path = self.service.icon_path(artifact_id)?.to_path_buf();
        image_response(path).await
    }

    async fn install_page(&self, artifact_id: &str) -> Result<Response<ResponseBody>, HttpError> {
        let artifact = self.service.viewable_artifact(artifact_id)?;
        let itms_url = self.service.itms_services_url(artifact);
        let icon_url = self.service.icon_url(artifact);
        let html = install_page::render(
            artifact,
            &itms_url,
            icon_url.as_deref(),
            self.service.availability(),
        );
        let mut response = text_response(StatusCode::OK, "text/html; charset=utf-8", html);
        response.headers_mut().insert(
            "content-security-policy",
            HeaderValue::from_static(
                "default-src 'none'; style-src 'unsafe-inline'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
            ),
        );
        response
            .headers_mut()
            .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
        response.headers_mut().insert(
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        );
        no_store(&mut response);
        Ok(response)
    }
}

/// Run the loopback origin used by one share session until the caller's
/// shutdown future resolves. There is deliberately no public `serve` mode or
/// TLS certificate configuration: the selected tunnel terminates HTTPS.
pub async fn run_listener(
    listener: TcpListener,
    state: HttpState,
    shutdown: impl std::future::Future<Output = ()> + Send,
) -> Result<(), BoxError> {
    let listen = listener.local_addr()?;
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    // Tells every live connection to finish its current request and close.
    // Without it, a browser holding an idle keep-alive socket open would pin
    // the drain for its full timeout even though nothing is being served —
    // which would quietly turn `--timeout 300` into "closes at 420".
    let (close_connections, _) = broadcast::channel::<()>(1);
    tracing::info!(address = %listen, "remote-installer origin listening");
    let accept_connections = accept_loop(
        listener,
        state,
        connections.clone(),
        close_connections.clone(),
    );
    tokio::pin!(accept_connections);
    tokio::pin!(shutdown);
    tokio::select! {
        result = &mut accept_connections => result,
        () = &mut shutdown => {
            let _ = close_connections.send(());
            drain(&connections).await;
            Ok(())
        }
    }
}

/// Stop accepting and wait for every live connection to finish, so an IPA
/// download in progress is allowed to complete.
async fn drain(connections: &Semaphore) {
    let in_flight = MAX_CONNECTIONS - connections.available_permits();
    if in_flight == 0 {
        return;
    }
    tracing::info!(in_flight, "draining in-flight connections");
    println!(
        "Finishing {in_flight} in-flight request(s) before shutting down (up to {}s)...",
        GRACEFUL_DRAIN_TIMEOUT.as_secs()
    );
    match tokio::time::timeout(
        GRACEFUL_DRAIN_TIMEOUT,
        connections.acquire_many(MAX_CONNECTIONS as u32),
    )
    .await
    {
        Ok(_) => tracing::info!("all connections drained"),
        Err(_) => tracing::warn!(
            "drain timed out; {} connection(s) were cut off",
            MAX_CONNECTIONS - connections.available_permits()
        ),
    }
}

async fn accept_loop(
    listener: TcpListener,
    state: HttpState,
    connections: Arc<Semaphore>,
    close_connections: broadcast::Sender<()>,
) -> Result<(), BoxError> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            // A transient accept failure (a descriptor limit, a connection
            // reset between the SYN and the accept) must not take the whole
            // origin down with it.
            Err(error) => {
                tracing::warn!(%error, "accept failed; continuing to listen");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        // Acquiring the permit after accepting (rather than before) means a
        // saturated pool sheds the new connection instead of leaving it
        // parked in the kernel's accept queue, where it would sit idle for up
        // to HEADER_READ_TIMEOUT and starve real downloads.
        let permit = match connections.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                tracing::warn!(
                    %peer,
                    "shedding connection: concurrency limit reached"
                );
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let state = state.clone();
        let mut closing = close_connections.subscribe();
        tokio::spawn(async move {
            let _permit = permit;
            let result = async {
                // `http1_only` keeps the previous behaviour; the settings are
                // applied through the parent builder rather than the `http1()`
                // sub-builder because only the parent hands back a connection
                // that can be told to shut down gracefully.
                let mut connection_builder =
                    ConnectionBuilder::new(TokioExecutor::new()).http1_only();
                // hyper panics on every connection if a timeout is configured
                // without a timer to drive it.
                connection_builder
                    .http1()
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_READ_TIMEOUT);
                let service = service_fn(move |request| {
                    let state = state.clone();
                    async move { state.handle(request).await }
                });
                let connection = connection_builder.serve_connection(TokioIo::new(stream), service);
                tokio::pin!(connection);
                loop {
                    tokio::select! {
                        result = connection.as_mut() => break result?,
                        // An idle keep-alive connection ends here; one that is
                        // mid-download keeps streaming until the body is done,
                        // which is exactly what the drain is waiting for.
                        _ = closing.recv() => connection.as_mut().graceful_shutdown(),
                    }
                }
                Ok::<(), BoxError>(())
            }
            .await;
            if let Err(error) = result {
                tracing::debug!(%error, %peer, "HTTP connection ended");
            }
        });
    }
}

/// A single `bytes=first-last` range resolved against a known entity length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: u64,
    end_inclusive: u64,
}

impl ByteRange {
    fn length(&self) -> u64 {
        self.end_inclusive - self.start + 1
    }
}

/// Parse a single-range `Range` header. Multi-range requests return `None`, so
/// the caller falls back to sending the whole entity, which is always valid.
fn parse_range(header: &str, total: u64) -> Option<Result<ByteRange, ()>> {
    let spec = header.trim().strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None;
    }
    let (first, last) = spec.split_once('-')?;
    let (first, last) = (first.trim(), last.trim());
    if total == 0 {
        return Some(Err(()));
    }
    let range = if first.is_empty() {
        // `bytes=-N`: the final N bytes.
        let suffix: u64 = last.parse().ok()?;
        if suffix == 0 {
            return Some(Err(()));
        }
        ByteRange {
            start: total.saturating_sub(suffix),
            end_inclusive: total - 1,
        }
    } else {
        let start: u64 = first.parse().ok()?;
        let end_inclusive = if last.is_empty() {
            total - 1
        } else {
            last.parse::<u64>().ok()?.min(total - 1)
        };
        if start > end_inclusive || start >= total {
            return Some(Err(()));
        }
        ByteRange {
            start,
            end_inclusive,
        }
    };
    Some(Ok(range))
}

/// Strip anything that could escape the quoted `filename="..."` parameter,
/// and fold non-ASCII characters down so the result is always a valid
/// `HeaderValue`. This is only ever the fallback: the real name (accents,
/// CJK, whatever the artifact was actually called) still reaches the client
/// through the `filename*=` parameter `content_disposition_value` adds
/// alongside it.
/// Drop anything that could escape the quoted `filename="..."` parameter or be
/// read as a path separator by whatever saves the file.
///
/// Both `Content-Disposition` parameters are built from this, so the RFC 8187
/// form cannot percent-encode a separator back into a name the quoted form
/// deliberately strips: `%2F` decodes to `/` just as well as `/` does.
fn sanitize_filename_characters(file_name: &str) -> String {
    let sanitized = file_name
        .chars()
        .filter(|character| {
            !character.is_control() && !matches!(character, '"' | '\\' | '/' | '\r' | '\n')
        })
        .collect::<String>();
    if sanitized.trim().is_empty() {
        "artifact.ipa".to_string()
    } else {
        sanitized
    }
}

/// The `filename="..."` fallback, which must additionally be pure ASCII to be
/// a legal header value at all.
fn sanitize_filename(file_name: &str) -> String {
    sanitize_filename_characters(file_name)
        .chars()
        .map(|character| if character.is_ascii() { character } else { '_' })
        .collect()
}

/// Percent-encode every byte that is not an RFC 8187 `attr-char`, for the
/// `filename*=` parameter of `Content-Disposition`. `attr-char` is narrower
/// than general URL percent-encoding — it also excludes `*`, `'`, and `%` —
/// so a general-purpose URL encoder cannot be reused here.
fn percent_encode_attr_char(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Build the `Content-Disposition` value for a downloaded file. A client that
/// understands RFC 8187's `filename*=` (RFC 6266 says to prefer it when
/// present) gets the artifact's real name, accents and all; everything else
/// falls back to the ASCII-folded `filename=`. The extended parameter is
/// only added when it would actually add information, so the common
/// all-ASCII case is byte-identical to a plain `filename=`.
fn content_disposition_value(file_name: &str) -> String {
    let ascii_fallback = sanitize_filename(file_name);
    let encoded = percent_encode_attr_char(&sanitize_filename_characters(file_name));
    if encoded == ascii_fallback {
        format!("attachment; filename=\"{ascii_fallback}\"")
    } else {
        format!("attachment; filename=\"{ascii_fallback}\"; filename*=UTF-8''{encoded}")
    }
}

/// Tracks one IPA transfer so the person who ran `share` can watch it happen.
///
/// After the QR code is printed the terminal would otherwise go silent for the
/// entire download, leaving no way to tell a phone pulling 200 MB from a phone
/// that never connected — or to know when it is safe to press Ctrl-C.
///
/// The reporter lives inside the response body's stream closure, so it is
/// dropped exactly when the body is: a transfer that ends without delivering
/// every byte reports itself interrupted from `Drop`, with no separate
/// bookkeeping to keep in sync.
struct DownloadProgress {
    file_name: String,
    /// Bytes this response promised, which for a `Range` request is the length
    /// of the range rather than of the file.
    expected: u64,
    is_range: bool,
    started: Instant,
    sent: u64,
    last_report: Instant,
    /// Set once the completion line is emitted, so `Drop` stays quiet.
    finished: bool,
}

impl DownloadProgress {
    fn new(file_name: &str, expected: u64, is_range: bool, now: Instant) -> Self {
        Self {
            file_name: file_name.to_string(),
            expected,
            is_range,
            started: now,
            sent: 0,
            last_report: now,
            finished: false,
        }
    }

    /// Record bytes handed to the client, returning the line to print, if any.
    fn record(&mut self, count: u64, now: Instant) -> Option<String> {
        self.sent = self.sent.saturating_add(count);
        if self.expected > 0 && self.sent >= self.expected && !self.finished {
            self.finished = true;
            let scope = if self.is_range { " range" } else { "" };
            return Some(format!(
                "Download complete: {} ({}{scope} in {})",
                self.file_name,
                format_bytes(self.sent),
                format_elapsed(now.saturating_duration_since(self.started)),
            ));
        }
        if self.expected == 0
            || now.saturating_duration_since(self.last_report) < PROGRESS_REPORT_INTERVAL
        {
            return None;
        }
        self.last_report = now;
        Some(format!(
            "Downloading {}: {}% ({} / {})",
            self.file_name,
            self.percent(),
            format_bytes(self.sent),
            format_bytes(self.expected),
        ))
    }

    /// The line to print if the transfer ends here, or `None` when it already
    /// delivered everything it promised.
    fn interrupted(&self) -> Option<String> {
        if self.finished || self.sent >= self.expected {
            return None;
        }
        Some(format!(
            "Download interrupted: {} at {}% ({} / {})",
            self.file_name,
            self.percent(),
            format_bytes(self.sent),
            format_bytes(self.expected),
        ))
    }

    fn percent(&self) -> u64 {
        if self.expected == 0 {
            return 0;
        }
        self.sent.saturating_mul(100) / self.expected
    }
}

impl Drop for DownloadProgress {
    fn drop(&mut self) {
        if let Some(line) = self.interrupted() {
            println!("{line}");
        }
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds < 10.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{seconds:.0}s")
    }
}

async fn file_response(
    path: PathBuf,
    file_name: &str,
    range_header: Option<&str>,
    report_progress: bool,
) -> Result<Response<ResponseBody>, HttpError> {
    let mut file = tokio::fs::File::open(&path).await?;
    let total = file.metadata().await?.len();
    let range = match range_header.and_then(|header| parse_range(header, total)) {
        Some(Ok(range)) => Some(range),
        Some(Err(())) => return Err(HttpError::RangeNotSatisfiable),
        None => None,
    };
    if let Some(range) = range {
        file.seek(std::io::SeekFrom::Start(range.start)).await?;
    }
    let length = range.map_or(total, |range| range.length());
    let reader = tokio::io::AsyncReadExt::take(file, length);

    let reader_stream = ReaderStream::new(reader);
    // A HEAD has its body discarded by `empty_body`, so instrumenting it would
    // report a download that never happened.
    let body = if report_progress {
        let mut progress =
            DownloadProgress::new(file_name, length, range.is_some(), Instant::now());
        StreamBody::new(futures_util::StreamExt::map(
            reader_stream,
            move |chunk| -> Result<Frame<Bytes>, BoxError> {
                let bytes = chunk.map_err(|error| -> BoxError { Box::new(error) })?;
                if let Some(line) = progress.record(bytes.len() as u64, Instant::now()) {
                    println!("{line}");
                }
                Ok(Frame::data(bytes))
            },
        ))
        .boxed()
    } else {
        StreamBody::new(futures_util::StreamExt::map(reader_stream, |chunk| {
            chunk
                .map(Frame::data)
                .map_err(|error| -> BoxError { Box::new(error) })
        }))
        .boxed()
    };

    let mut response = Response::new(body);
    if let Some(range) = range {
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
        if let Ok(value) = HeaderValue::from_str(&format!(
            "bytes {}-{}/{total}",
            range.start, range.end_inclusive
        )) {
            response.headers_mut().insert(CONTENT_RANGE, value);
        }
    }
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    // iOS renders a real progress ring during OTA installation only when it
    // knows the size up front, and Accept-Ranges lets an interrupted download
    // resume instead of restarting.
    response
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from(length));
    response
        .headers_mut()
        .insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response.headers_mut().insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&content_disposition_value(file_name))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    // Quota enforcement happens in this process. Do not let a tunnel or
    // intermediary replay a cached IPA without reaching `authorize_download`.
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query?.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

async fn image_response(path: PathBuf) -> Result<Response<ResponseBody>, HttpError> {
    let file = tokio::fs::File::open(&path).await?;
    let length = file.metadata().await?.len();
    let stream = futures_util::StreamExt::map(ReaderStream::new(file), |chunk| {
        chunk
            .map(Frame::data)
            .map_err(|error| -> BoxError { Box::new(error) })
    });
    let body = StreamBody::new(stream).boxed();
    let mut response = Response::new(body);
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("image/png"));
    response
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from(length));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        "cache-control",
        HeaderValue::from_static("public, max-age=3600, immutable"),
    );
    Ok(response)
}

fn no_store(response: &mut Response<ResponseBody>) {
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
}

/// Drop the body of an already-built response while keeping its status and
/// every header exactly as the equivalent GET produced them. Used to turn any
/// GET response (success or error) into the HEAD response for the same
/// request.
fn empty_body(response: Response<ResponseBody>) -> Response<ResponseBody> {
    let (parts, _body) = response.into_parts();
    Response::from_parts(
        parts,
        Empty::new()
            .map_err(|never: Infallible| -> BoxError { match never {} })
            .boxed(),
    )
}

/// A known path requested with a method other than GET or HEAD is a 405, not
/// a 404: the resource exists, only the verb is wrong.
fn method_not_allowed_response() -> Response<ResponseBody> {
    let mut response = json_response(
        StatusCode::METHOD_NOT_ALLOWED,
        serde_json::json!({"error": "method not allowed"}),
    );
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    response
}

fn json_response(status: StatusCode, value: serde_json::Value) -> Response<ResponseBody> {
    let body = Full::new(Bytes::from(value.to_string()))
        .map_err(|never: Infallible| -> BoxError { match never {} })
        .boxed();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn text_response(status: StatusCode, content_type: &str, value: String) -> Response<ResponseBody> {
    let body = Full::new(Bytes::from(value))
        .map_err(|never: Infallible| -> BoxError { match never {} })
        .boxed();
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("text/plain")),
    );
    response
}

fn error_response(error: HttpError) -> Response<ResponseBody> {
    let status = match &error {
        HttpError::Service(ServiceError::NotFound(_)) => StatusCode::NOT_FOUND,
        HttpError::Service(ServiceError::Gone(_)) => StatusCode::GONE,
        HttpError::Service(ServiceError::Forbidden(_)) => StatusCode::FORBIDDEN,
        HttpError::RangeNotSatisfiable => StatusCode::RANGE_NOT_SATISFIABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if status.is_server_error() {
        tracing::error!(%error, "request failed");
    }
    json_response(status, serde_json::json!({"error": error.to_string()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn image_response_serves_png_with_security_headers() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let bytes = b"\x89PNG\r\n\x1a\nicon";
        std::fs::write(file.path(), bytes).unwrap();

        let response = image_response(file.path().to_path_buf()).await.unwrap();
        assert_eq!(response.headers()[CONTENT_TYPE], "image/png");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()[CONTENT_LENGTH], "12");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), bytes);
    }

    #[tokio::test]
    async fn download_advertises_length_and_serves_ranges() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"0123456789").unwrap();

        let whole = file_response(file.path().to_path_buf(), "App.ipa", None, false)
            .await
            .unwrap();
        assert_eq!(whole.status(), StatusCode::OK);
        assert_eq!(whole.headers()[CONTENT_LENGTH], "10");
        assert_eq!(whole.headers()[ACCEPT_RANGES], "bytes");
        assert_eq!(whole.headers()[CACHE_CONTROL], "no-store");

        let partial = file_response(
            file.path().to_path_buf(),
            "App.ipa",
            Some("bytes=2-5"),
            false,
        )
        .await
        .unwrap();
        assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(partial.headers()[CONTENT_RANGE], "bytes 2-5/10");
        assert_eq!(partial.headers()[CONTENT_LENGTH], "4");
        let body = partial.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), b"2345");

        let suffix = file_response(
            file.path().to_path_buf(),
            "App.ipa",
            Some("bytes=-3"),
            false,
        )
        .await
        .unwrap();
        let body = suffix.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), b"789");

        let unsatisfiable = file_response(
            file.path().to_path_buf(),
            "App.ipa",
            Some("bytes=99-"),
            false,
        )
        .await;
        assert!(matches!(unsatisfiable, Err(HttpError::RangeNotSatisfiable)));
    }

    #[test]
    fn range_parsing_handles_the_shapes_ios_sends() {
        assert_eq!(
            parse_range("bytes=0-499", 1000),
            Some(Ok(ByteRange {
                start: 0,
                end_inclusive: 499
            }))
        );
        assert_eq!(
            parse_range("bytes=500-", 1000),
            Some(Ok(ByteRange {
                start: 500,
                end_inclusive: 999
            }))
        );
        // A last-byte-pos past the end is clamped, per RFC 9110.
        assert_eq!(
            parse_range("bytes=900-5000", 1000),
            Some(Ok(ByteRange {
                start: 900,
                end_inclusive: 999
            }))
        );
        assert_eq!(parse_range("bytes=1000-", 1000), Some(Err(())));
        // Multi-range and malformed headers fall back to the whole entity.
        assert_eq!(parse_range("bytes=0-1,5-6", 1000), None);
        assert_eq!(parse_range("items=0-1", 1000), None);
    }

    #[test]
    fn content_disposition_filenames_cannot_break_out_of_the_quotes() {
        assert_eq!(sanitize_filename("App.ipa"), "App.ipa");
        assert_eq!(
            sanitize_filename("evil\"; download=\"x.ipa"),
            "evil; download=x.ipa"
        );
        assert_eq!(sanitize_filename("../../etc/passwd"), "....etcpasswd");
        assert_eq!(sanitize_filename("   "), "artifact.ipa");
    }

    #[test]
    fn content_disposition_value_only_adds_the_extended_parameter_when_needed() {
        // All-ASCII names round-trip through `attr-char` unchanged, so the
        // extended parameter would add nothing: the header stays exactly
        // what it was before RFC 8187 support existed.
        assert_eq!(
            content_disposition_value("App.ipa"),
            "attachment; filename=\"App.ipa\""
        );
        // A non-ASCII name gets an ASCII-folded fallback plus the real name,
        // percent-encoded per RFC 8187, so clients that understand
        // `filename*=` show the app's actual name instead of underscores.
        assert_eq!(
            content_disposition_value("我的应用.ipa"),
            "attachment; filename=\"____.ipa\"; filename*=UTF-8''%E6%88%91%E7%9A%84%E5%BA%94%E7%94%A8.ipa"
        );
        // The extended parameter is built from the same sanitized name as the
        // quoted one, so it cannot percent-encode a path separator or a quote
        // back into a filename that the quoted form strips.
        let smuggled = content_disposition_value("../../etc/passwd\"; x=\"y");
        assert!(!smuggled.contains("%2F"), "{smuggled}");
        assert!(!smuggled.contains("%22"), "{smuggled}");
        assert!(!smuggled.contains("%5C"), "{smuggled}");
    }

    #[tokio::test]
    async fn head_response_keeps_headers_but_drops_the_body() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"0123456789").unwrap();

        let get = file_response(file.path().to_path_buf(), "App.ipa", None, false)
            .await
            .unwrap();
        let content_length = get.headers()[CONTENT_LENGTH].clone();
        let content_type = get.headers()[CONTENT_TYPE].clone();

        let head = empty_body(get);
        assert_eq!(head.headers()[CONTENT_LENGTH], content_length);
        assert_eq!(head.headers()[CONTENT_TYPE], content_type);
        let body = head.into_body().collect().await.unwrap().to_bytes();
        assert!(body.is_empty());
    }

    #[test]
    fn only_the_four_ota_resources_are_exposed() {
        assert!(matches!(
            route(&Method::GET, "/install/artifact-1"),
            Route::Found(Resource::InstallPage(_))
        ));
        assert!(matches!(
            route(&Method::GET, "/api/v1/artifacts/artifact-1/icon.png"),
            Route::Found(Resource::Icon(_))
        ));
        assert!(matches!(
            route(&Method::GET, "/api/v1/artifacts/artifact-1/download.ipa"),
            Route::Found(Resource::Download(_))
        ));
        assert!(matches!(
            route(&Method::GET, "/api/v1/artifacts/artifact-1/manifest.plist"),
            Route::Found(Resource::Manifest(_))
        ));

        assert!(matches!(route(&Method::GET, "/healthz"), Route::NotFound));
        assert!(matches!(
            route(&Method::GET, "/api/v1/artifacts"),
            Route::NotFound
        ));
    }

    #[test]
    fn head_is_supported_on_every_route_and_other_methods_are_405() {
        for path in [
            "/install/artifact-1",
            "/api/v1/artifacts/artifact-1/icon.png",
            "/api/v1/artifacts/artifact-1/download.ipa",
            "/api/v1/artifacts/artifact-1/manifest.plist",
        ] {
            assert!(
                matches!(route(&Method::HEAD, path), Route::Found(_)),
                "HEAD {path} should resolve to the same resource as GET"
            );
            assert!(
                matches!(route(&Method::POST, path), Route::MethodNotAllowed),
                "POST {path} should be 405, not 404"
            );
        }
        // A path that matches no route is still a 404 regardless of method.
        assert!(matches!(route(&Method::POST, "/healthz"), Route::NotFound));
    }

    #[test]
    fn a_completed_download_reports_completion_once_and_never_interruption() {
        let start = Instant::now();
        let mut progress = DownloadProgress::new("App.ipa", 10, false, start);

        assert_eq!(progress.record(4, start), None);
        let line = progress.record(6, start).expect("completion line");
        assert!(
            line.starts_with("Download complete: App.ipa (10 B in "),
            "{line}"
        );
        // Completion is emitted exactly once, and a dropped body stays quiet.
        assert_eq!(progress.record(0, start), None);
        assert_eq!(progress.interrupted(), None);
    }

    #[test]
    fn an_abandoned_download_reports_where_it_stopped() {
        let start = Instant::now();
        let mut progress = DownloadProgress::new("App.ipa", 1000, false, start);
        progress.record(620, start);

        assert_eq!(
            progress.interrupted().as_deref(),
            Some("Download interrupted: App.ipa at 62% (620 B / 1.0 KB)")
        );
    }

    #[test]
    fn progress_lines_are_rate_limited() {
        let start = Instant::now();
        let mut progress = DownloadProgress::new("App.ipa", 1_000_000, false, start);

        // Two chunks milliseconds apart are one transfer, not two lines.
        assert_eq!(
            progress.record(1_000, start + Duration::from_millis(5)),
            None
        );
        assert_eq!(
            progress.record(1_000, start + Duration::from_millis(10)),
            None
        );

        let later = start + PROGRESS_REPORT_INTERVAL + Duration::from_millis(1);
        let line = progress.record(448_000, later).expect("progress line");
        assert_eq!(line, "Downloading App.ipa: 45% (450.0 KB / 1.0 MB)");
        // ...and the next chunk right after it is rate-limited again.
        assert_eq!(progress.record(1_000, later), None);
    }

    #[test]
    fn a_range_response_reports_against_the_range_not_the_file() {
        let start = Instant::now();
        let mut progress = DownloadProgress::new("App.ipa", 4_000_000, true, start);
        let line = progress.record(4_000_000, start).expect("completion line");
        assert!(line.contains("4.0 MB range in"), "{line}");
    }

    /// A zero-length body has nothing to report and must not look interrupted.
    #[test]
    fn an_empty_response_reports_nothing() {
        let start = Instant::now();
        let progress = DownloadProgress::new("App.ipa", 0, false, start);
        assert_eq!(progress.interrupted(), None);
    }

    #[tokio::test]
    async fn an_instrumented_download_still_serves_identical_bytes() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"0123456789").unwrap();

        let reported = file_response(file.path().to_path_buf(), "App.ipa", None, true)
            .await
            .unwrap();
        assert_eq!(reported.headers()[CONTENT_LENGTH], "10");
        let body = reported.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), b"0123456789");
    }
}
