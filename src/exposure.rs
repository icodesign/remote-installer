use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout};
use url::Url;

const TAILSCALE_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
const TAILSCALE_POLL_INTERVAL: Duration = Duration::from_millis(500);
const TAILSCALE_DNS_STATUS_TIMEOUT: Duration = Duration::from_secs(5);

const TAILSCALE_APP_CLI: &str = "/Applications/Tailscale.app/Contents/MacOS/Tailscale";
const CLOUDFLARED_HOMEBREW_CLI: &str = "/opt/homebrew/bin/cloudflared";
const CLOUDFLARED_USR_LOCAL_CLI: &str = "/usr/local/bin/cloudflared";
const CLOUDFLARE_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const CLOUDFLARE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_CAPTURED_LOG_LINES: usize = 40;
const CLOUDFLARE_START_ATTEMPTS: u32 = 3;

const CLOUDFLARED_INSTALL_HINT: &str = "
install cloudflared with one of the following:
  - brew install cloudflared
  - download a macOS arm64/amd64 binary from https://github.com/cloudflare/cloudflared/releases
  - see Cloudflare's docs: https://developers.cloudflare.com/cloudflare-tunnel/downloads/

No Cloudflare account and no `cloudflared login` are required for the Quick Tunnel this tool uses.
If cloudflared is installed somewhere else, pass --cloudflared-bin /path/to/cloudflared.
Already checked: $PATH, /opt/homebrew/bin, /usr/local/bin.";

const TAILSCALE_INSTALL_HINT: &str = "
install Tailscale with one of the following:
  - brew install --cask tailscale
  - the Mac App Store, or https://tailscale.com/download

Note: Tailscale Serve and Funnel require a Tailscale account. Serve keeps the link private to
your tailnet; Funnel exposes it publicly and also requires Funnel to be enabled. If you'd
rather not create an account, pass --provider cloudflare to use the Cloudflare Quick Tunnel.
If Tailscale is installed somewhere else, pass --tailscale-bin /path/to/tailscale.
Already checked: $PATH, /Applications/Tailscale.app/Contents/MacOS/Tailscale.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExposureProvider {
    TailscaleServe,
    TailscaleFunnel,
    /// Compatibility alias for the former single Tailscale provider, which
    /// exposed a public Funnel URL.
    Tailscale,
    Cloudflare,
}

impl ExposureProvider {
    pub fn name(self) -> &'static str {
        match self {
            Self::TailscaleServe => "Tailscale Serve",
            Self::TailscaleFunnel | Self::Tailscale => "Tailscale Funnel",
            Self::Cloudflare => "Cloudflare Quick Tunnel",
        }
    }

    fn tailscale_mode(self) -> Option<TailscaleMode> {
        match self {
            Self::TailscaleServe => Some(TailscaleMode::Serve),
            Self::TailscaleFunnel | Self::Tailscale => Some(TailscaleMode::Funnel),
            Self::Cloudflare => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailscaleMode {
    Serve,
    Funnel,
}

impl TailscaleMode {
    fn command(self) -> &'static str {
        match self {
            Self::Serve => "serve",
            Self::Funnel => "funnel",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Serve => "Serve",
            Self::Funnel => "Funnel",
        }
    }
}

#[derive(Debug, Error)]
pub enum ExposureError {
    #[error("{display_name} CLI was not found.{install_hint}")]
    CliNotFound {
        display_name: &'static str,
        install_hint: &'static str,
    },
    #[error("tunnel process I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("Tailscale returned invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Tailscale is not ready: {0}")]
    TailscaleNotReady(String),
    #[error("an existing Tailscale {mode} configuration is active; refusing to replace it")]
    ExistingTailscaleConfiguration { mode: &'static str },
    #[error("Tailscale {mode} requires a valid HTTPS port, got {port}")]
    InvalidTailscalePort { mode: &'static str, port: u16 },
    #[error("{program} failed: {message}")]
    Command {
        program: &'static str,
        message: String,
    },
    #[error("tunnel provider reported an invalid public URL: {0}")]
    InvalidPublicUrl(String),
    #[error("Tailscale session exited: {0}")]
    TailscaleExited(String),
    #[error(
        "Tailscale did not become ready within 120 seconds; complete the setup shown above and retry{0}"
    )]
    TailscaleStartupTimeout(String),
    #[error("Cloudflare Quick Tunnel did not become ready within 30 seconds{0}")]
    CloudflareStartupTimeout(String),
    #[error("cloudflared exited before the sharing session ended{0}")]
    CloudflareExited(String),
    #[error("Cloudflare metrics client failed: {0}")]
    Http(#[from] reqwest::Error),
}

pub struct ExposureSession {
    provider: ExposureProvider,
    public_base_url: Url,
    warnings: Vec<String>,
    inner: ExposureSessionInner,
}

enum ExposureSessionInner {
    Tailscale(TailscaleSession),
    Cloudflare(CloudflareSession),
}

struct TailscaleSession {
    child: Child,
    captured_logs: CapturedLogs,
    log_tasks: Vec<JoinHandle<()>>,
}

struct CloudflareSession {
    child: Child,
    captured_logs: CapturedLogs,
    log_task: Option<JoinHandle<()>>,
}

#[derive(Clone, Default)]
struct CapturedLogs(Arc<Mutex<VecDeque<String>>>);

#[derive(Debug, Deserialize)]
struct TailscaleStatus {
    #[serde(rename = "BackendState")]
    backend_state: String,
    #[serde(rename = "Self")]
    self_node: Option<TailscaleNode>,
}

#[derive(Debug, Deserialize)]
struct TailscaleNode {
    #[serde(rename = "DNSName")]
    dns_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TailscaleDnsStatus {
    #[serde(rename = "TailscaleDNS")]
    tailscale_dns: bool,
    #[serde(rename = "CurrentTailnet")]
    current_tailnet: TailscaleCurrentTailnet,
}

#[derive(Debug, Deserialize)]
struct TailscaleCurrentTailnet {
    #[serde(rename = "MagicDNSEnabled")]
    magic_dns_enabled: bool,
}

#[derive(Debug, Deserialize)]
struct QuickTunnelStatus {
    hostname: String,
}

impl ExposureSession {
    pub async fn start(
        provider: ExposureProvider,
        target: &Url,
        tailscale_binary: Option<&Path>,
        cloudflared_binary: Option<&Path>,
        tailscale_https_port: u16,
    ) -> Result<Self, ExposureError> {
        Self::start_with_options(
            provider,
            target,
            tailscale_binary,
            cloudflared_binary,
            tailscale_https_port,
            true,
        )
        .await
    }

    /// Start a provider after the caller has performed one shared Tailscale
    /// preflight. Auto mode uses this for its parallel Serve/Funnel starts so
    /// the second child does not mistake the first child that this same command
    /// just created for a pre-existing user configuration.
    pub async fn start_without_configuration_check(
        provider: ExposureProvider,
        target: &Url,
        tailscale_binary: Option<&Path>,
        cloudflared_binary: Option<&Path>,
        tailscale_https_port: u16,
    ) -> Result<Self, ExposureError> {
        Self::start_with_options(
            provider,
            target,
            tailscale_binary,
            cloudflared_binary,
            tailscale_https_port,
            false,
        )
        .await
    }

    async fn start_with_options(
        provider: ExposureProvider,
        target: &Url,
        tailscale_binary: Option<&Path>,
        cloudflared_binary: Option<&Path>,
        tailscale_https_port: u16,
        check_existing_tailscale_configuration: bool,
    ) -> Result<Self, ExposureError> {
        match provider {
            ExposureProvider::TailscaleServe
            | ExposureProvider::TailscaleFunnel
            | ExposureProvider::Tailscale => {
                let mode = provider
                    .tailscale_mode()
                    .expect("matched a Tailscale provider");
                start_tailscale(
                    mode,
                    target,
                    tailscale_binary,
                    tailscale_https_port,
                    check_existing_tailscale_configuration,
                )
                .await
            }
            ExposureProvider::Cloudflare => start_cloudflare(target, cloudflared_binary).await,
        }
    }

    /// Check that the Tailscale CLI can serve this process and that no
    /// existing Serve/Funnel configuration would be overwritten by auto mode.
    pub async fn check_tailscale_for_auto(
        binary_override: Option<&Path>,
    ) -> Result<(), ExposureError> {
        let binary = discover_binary(
            "Tailscale",
            "tailscale",
            binary_override,
            &[TAILSCALE_APP_CLI],
            TAILSCALE_INSTALL_HINT,
        )?;
        let status: TailscaleStatus = command_json(&binary, &["status", "--json"]).await?;
        if status.backend_state != "Running" {
            return Err(ExposureError::TailscaleNotReady(format!(
                "backend state is {}",
                status.backend_state
            )));
        }
        status
            .self_node
            .and_then(|node| node.dns_name)
            .filter(|name| !name.trim_matches('.').is_empty())
            .ok_or_else(|| {
                ExposureError::TailscaleNotReady("the current node has no MagicDNS name".into())
            })?;
        // Check both modes before either child is spawned. Serve and Funnel
        // keep separate foreground entries, so checking only Serve could let
        // auto mode overwrite a user's existing Funnel configuration.
        ensure_empty_tailscale_configuration(&binary, TailscaleMode::Serve).await?;
        ensure_empty_tailscale_configuration(&binary, TailscaleMode::Funnel).await
    }

    pub fn provider(&self) -> ExposureProvider {
        self.provider
    }

    pub fn public_base_url(&self) -> &Url {
        &self.public_base_url
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub async fn wait_for_exit(&mut self) -> Result<(), ExposureError> {
        match &mut self.inner {
            ExposureSessionInner::Tailscale(session) => session.wait_for_exit().await,
            ExposureSessionInner::Cloudflare(session) => session.wait_for_exit().await,
        }
    }

    pub async fn stop(&mut self) -> Result<(), ExposureError> {
        match &mut self.inner {
            ExposureSessionInner::Tailscale(session) => session.stop().await,
            ExposureSessionInner::Cloudflare(session) => session.stop().await,
        }
    }
}

impl TailscaleSession {
    async fn wait_for_exit(&mut self) -> Result<(), ExposureError> {
        let status = self.child.wait().await?;
        self.finish_logs().await;
        Err(ExposureError::TailscaleExited(format!(
            "{status}\n{}",
            self.captured_logs.snapshot().await
        )))
    }

    async fn stop(&mut self) -> Result<(), ExposureError> {
        if self.child.try_wait()?.is_none() {
            self.child.start_kill()?;
        }
        self.child.wait().await?;
        self.finish_logs().await;
        Ok(())
    }

    async fn finish_logs(&mut self) {
        for task in self.log_tasks.drain(..) {
            let _ = task.await;
        }
    }
}

impl Drop for TailscaleSession {
    fn drop(&mut self) {
        // Foreground config belongs to this child's IPN bus session. Killing
        // the child closes that session, including when startup is cancelled.
        let _ = self.child.start_kill();
        for task in &self.log_tasks {
            task.abort();
        }
    }
}

fn tailscale_log_task(
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    captured: CapturedLogs,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            // In particular, HTTPS/Funnel first-use authorization URLs must
            // reach the user before the child completes its setup.
            eprintln!("{line}");
            captured.push(line).await;
        }
    })
}

impl CloudflareSession {
    async fn wait_for_exit(&mut self) -> Result<(), ExposureError> {
        let status = self.child.wait().await?;
        self.finish_log_task().await;
        Err(ExposureError::CloudflareExited(format_diagnostic(
            status.to_string(),
            &self.captured_logs.snapshot().await,
        )))
    }

    async fn stop(&mut self) -> Result<(), ExposureError> {
        if self.child.try_wait()?.is_none() {
            self.child.start_kill()?;
            let _ = self.child.wait().await?;
        }
        self.finish_log_task().await;
        Ok(())
    }

    async fn finish_log_task(&mut self) {
        if let Some(task) = self.log_task.take() {
            let _ = task.await;
        }
    }
}

impl CapturedLogs {
    async fn push(&self, line: String) {
        let mut lines = self.0.lock().await;
        if lines.len() == MAX_CAPTURED_LOG_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    async fn snapshot(&self) -> String {
        self.0
            .lock()
            .await
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

async fn start_tailscale(
    mode: TailscaleMode,
    target: &Url,
    binary_override: Option<&Path>,
    https_port: u16,
    check_existing_tailscale_configuration: bool,
) -> Result<ExposureSession, ExposureError> {
    validate_tailscale_port(mode, https_port)?;
    let binary = discover_binary(
        "Tailscale",
        "tailscale",
        binary_override,
        &[TAILSCALE_APP_CLI],
        TAILSCALE_INSTALL_HINT,
    )?;
    let status: TailscaleStatus = command_json(&binary, &["status", "--json"]).await?;
    if status.backend_state != "Running" {
        return Err(ExposureError::TailscaleNotReady(format!(
            "backend state is {}",
            status.backend_state
        )));
    }
    let dns_name = status
        .self_node
        .and_then(|node| node.dns_name)
        .filter(|name| !name.trim_matches('.').is_empty())
        .ok_or_else(|| {
            ExposureError::TailscaleNotReady("the current node has no MagicDNS name".into())
        })?;
    if check_existing_tailscale_configuration {
        ensure_empty_tailscale_configuration(&binary, mode).await?;
    }
    let warnings = tailscale_dns_diagnostics(&binary, mode).await?;

    let public_base_url = tailscale_public_url(&dns_name, https_port)?;
    let port_flag = format!("--https={https_port}");
    let mut child = Command::new(&binary)
        .args([mode.command(), "--yes", port_flag.as_str(), target.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let captured_logs = CapturedLogs::default();
    let log_tasks = vec![
        tailscale_log_task(
            child.stdout.take().expect("piped stdout"),
            captured_logs.clone(),
        ),
        tailscale_log_task(
            child.stderr.take().expect("piped stderr"),
            captured_logs.clone(),
        ),
    ];
    let mut session = TailscaleSession {
        child,
        captured_logs,
        log_tasks,
    };
    let readiness = async {
        loop {
            let current: Value =
                command_json(&binary, &[mode.command(), "status", "--json"]).await?;
            if tailscale_configuration_ready(&current, mode, &dns_name, https_port, target) {
                return Ok::<(), ExposureError>(());
            }
            sleep(TAILSCALE_POLL_INTERVAL).await;
        }
    };
    let ready = tokio::select! {
        result = readiness => result,
        result = session.wait_for_exit() => result,
        _ = sleep(TAILSCALE_STARTUP_TIMEOUT) => Err(ExposureError::TailscaleStartupTimeout(
            format!("\n{}", session.captured_logs.snapshot().await)
        )),
    };
    if let Err(error) = ready {
        session.stop().await?;
        return Err(error);
    }
    if let Some(status) = session.child.try_wait()? {
        return Err(ExposureError::TailscaleExited(status.to_string()));
    }
    Ok(ExposureSession {
        provider: match mode {
            TailscaleMode::Serve => ExposureProvider::TailscaleServe,
            TailscaleMode::Funnel => ExposureProvider::TailscaleFunnel,
        },
        public_base_url,
        warnings,
        inner: ExposureSessionInner::Tailscale(session),
    })
}

// Use structured daemon state, not localized human-facing output, as readiness.
// Only the exact ephemeral HTTPS proxy and requested visibility qualify.
fn tailscale_configuration_ready(
    config: &Value,
    mode: TailscaleMode,
    dns_name: &str,
    port: u16,
    target: &Url,
) -> bool {
    let host_port = format!("{}:{port}", dns_name.trim_end_matches('.'));
    config
        .get("Foreground")
        .and_then(Value::as_object)
        .is_some_and(|sessions| {
            sessions.values().any(|session| {
                let proxy = session["Web"][&host_port]["Handlers"]["/"]["Proxy"]
                    .as_str()
                    .and_then(|value| Url::parse(value).ok());
                session["TCP"][port.to_string()]["HTTPS"].as_bool() == Some(true)
                    && proxy.as_ref() == Some(target)
                    && session["AllowFunnel"][&host_port]
                        .as_bool()
                        .unwrap_or(false)
                        == (mode == TailscaleMode::Funnel)
            })
        })
}

fn validate_tailscale_port(mode: TailscaleMode, port: u16) -> Result<(), ExposureError> {
    let supported = mode == TailscaleMode::Serve || matches!(port, 443 | 8443 | 10000);
    if port == 0 || !supported {
        return Err(ExposureError::InvalidTailscalePort {
            mode: mode.display_name(),
            port,
        });
    }
    Ok(())
}

async fn start_cloudflare(
    target: &Url,
    binary_override: Option<&Path>,
) -> Result<ExposureSession, ExposureError> {
    let binary = discover_binary(
        "cloudflared",
        "cloudflared",
        binary_override,
        &[CLOUDFLARED_HOMEBREW_CLI, CLOUDFLARED_USR_LOCAL_CLI],
        CLOUDFLARED_INSTALL_HINT,
    )?;

    // The metrics port is learned by binding to 127.0.0.1:0, then immediately dropping the
    // listener so cloudflared can bind it instead. That leaves a small TOCTOU window in which
    // another process can steal the port out from under us, causing cloudflared to fail to bind
    // its management endpoint and exit right away. Retry a bounded number of times with a fresh
    // port whenever that specific failure is what we observed; anything else surfaces immediately.
    let mut attempt = 1;
    loop {
        match start_cloudflare_once(target, &binary).await {
            Ok(session) => return Ok(session),
            Err(error)
                if attempt < CLOUDFLARE_START_ATTEMPTS && is_metrics_bind_failure(&error) =>
            {
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn start_cloudflare_once(
    target: &Url,
    binary: &Path,
) -> Result<ExposureSession, ExposureError> {
    let metrics_listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;
    let metrics_address = metrics_listener.local_addr()?;
    drop(metrics_listener);

    let metrics = metrics_address.to_string();
    let arguments = cloudflared_arguments(&metrics, target);
    let mut child = Command::new(binary)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stderr = child.stderr.take().ok_or_else(|| ExposureError::Command {
        program: "cloudflared",
        message: "failed to capture diagnostic output".into(),
    })?;
    let captured_logs = CapturedLogs::default();
    let logs_for_task = captured_logs.clone();
    let log_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            logs_for_task.push(line).await;
        }
    });

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(300))
        .timeout(Duration::from_secs(1))
        .build()?;
    let quick_tunnel_url = format!("http://{metrics_address}/quicktunnel");
    let ready_url = format!("http://{metrics_address}/ready");
    let deadline = Instant::now() + CLOUDFLARE_STARTUP_TIMEOUT;
    let mut public_base_url = None;

    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            let _ = log_task.await;
            return Err(ExposureError::CloudflareExited(format_diagnostic(
                status.to_string(),
                &captured_logs.snapshot().await,
            )));
        }
        if public_base_url.is_none()
            && let Ok(response) = client.get(&quick_tunnel_url).send().await
            && response.status().is_success()
            && let Ok(status) = response.json::<QuickTunnelStatus>().await
            && !status.hostname.is_empty()
        {
            public_base_url = Some(cloudflare_public_url(&status.hostname)?);
        }
        if client
            .get(&ready_url)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
            && let Some(public_base_url) = public_base_url.take()
        {
            return Ok(ExposureSession {
                provider: ExposureProvider::Cloudflare,
                public_base_url,
                warnings: Vec::new(),
                inner: ExposureSessionInner::Cloudflare(CloudflareSession {
                    child,
                    captured_logs,
                    log_task: Some(log_task),
                }),
            });
        }
        sleep(CLOUDFLARE_POLL_INTERVAL).await;
    }

    let _ = child.start_kill();
    let _ = child.wait().await;
    let _ = log_task.await;
    Err(ExposureError::CloudflareStartupTimeout(format_logs(
        &captured_logs.snapshot().await,
    )))
}

async fn ensure_empty_tailscale_configuration(
    binary: &Path,
    mode: TailscaleMode,
) -> Result<(), ExposureError> {
    let current: Value = command_json(binary, &[mode.command(), "status", "--json"]).await?;
    if empty_configuration(&current) {
        Ok(())
    } else {
        Err(ExposureError::ExistingTailscaleConfiguration {
            mode: mode.display_name(),
        })
    }
}

async fn tailscale_dns_diagnostics(
    binary: &Path,
    mode: TailscaleMode,
) -> Result<Vec<String>, ExposureError> {
    let unavailable = |detail: String| {
        format!(
            "could not inspect Tailscale DNS configuration ({detail}); run `tailscale dns status --json` or update Tailscale"
        )
    };
    let status: TailscaleDnsStatus = match timeout(
        TAILSCALE_DNS_STATUS_TIMEOUT,
        command_json(binary, &["dns", "status", "--json"]),
    )
    .await
    {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            return Err(ExposureError::TailscaleNotReady(unavailable(
                error.to_string(),
            )));
        }
        Err(_) => {
            return Err(ExposureError::TailscaleNotReady(unavailable(
                "command timed out after 5 seconds".into(),
            )));
        }
    };

    if !status.current_tailnet.magic_dns_enabled && mode == TailscaleMode::Funnel {
        return Err(ExposureError::TailscaleNotReady(
            "Tailscale Funnel requires MagicDNS; enable MagicDNS in the Tailscale admin console DNS settings before using Funnel"
                .into(),
        ));
    }

    let mut warnings = Vec::new();
    if !status.current_tailnet.magic_dns_enabled {
        warnings.push(
            "MagicDNS is disabled for this tailnet. Tailscale Serve can start, but receiving devices must use Tailscale DNS or another correctly configured resolver for the Serve hostname; enable MagicDNS in the Tailscale admin console DNS settings."
                .into(),
        );
    }
    if mode == TailscaleMode::Serve && !status.tailscale_dns {
        warnings.push(
            "Tailscale DNS is disabled on this computer. Serve can start, but this computer may not resolve its Serve hostname; run `tailscale set --accept-dns=true`. Receiving devices such as your phone must use Tailscale DNS or another correctly configured resolver."
                .into(),
        );
    }
    Ok(warnings)
}

fn discover_binary(
    display_name: &'static str,
    executable_name: &str,
    binary_override: Option<&Path>,
    standard_paths: &[&str],
    install_hint: &'static str,
) -> Result<PathBuf, ExposureError> {
    resolve_binary(executable_name, binary_override, standard_paths).ok_or(
        ExposureError::CliNotFound {
            display_name,
            install_hint,
        },
    )
}

fn resolve_binary(
    executable_name: &str,
    binary_override: Option<&Path>,
    standard_paths: &[&str],
) -> Option<PathBuf> {
    if let Some(path) = binary_override {
        return path.is_file().then(|| path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join(executable_name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    standard_paths
        .iter()
        .map(PathBuf::from)
        .find(|candidate| candidate.is_file())
}

/// Whether the executable for a provider is present and can therefore be
/// attempted by default auto mode. Explicit provider selection still runs the
/// normal discovery path so a missing binary retains its detailed install hint.
pub fn provider_binary_available(
    provider: ExposureProvider,
    tailscale_binary: Option<&Path>,
    cloudflared_binary: Option<&Path>,
) -> bool {
    match provider {
        ExposureProvider::TailscaleServe
        | ExposureProvider::TailscaleFunnel
        | ExposureProvider::Tailscale => {
            resolve_binary("tailscale", tailscale_binary, &[TAILSCALE_APP_CLI]).is_some()
        }
        ExposureProvider::Cloudflare => resolve_binary(
            "cloudflared",
            cloudflared_binary,
            &[CLOUDFLARED_HOMEBREW_CLI, CLOUDFLARED_USR_LOCAL_CLI],
        )
        .is_some(),
    }
}

async fn command_json<T: serde::de::DeserializeOwned>(
    binary: &Path,
    arguments: &[&str],
) -> Result<T, ExposureError> {
    let output = run_checked("Tailscale", binary, arguments).await?;
    Ok(serde_json::from_slice(&output.stdout)?)
}

async fn run_checked(
    program: &'static str,
    binary: &Path,
    arguments: &[&str],
) -> Result<Output, ExposureError> {
    let output = Command::new(binary)
        .args(arguments)
        .kill_on_drop(true)
        .output()
        .await?;
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() { stdout } else { stderr };
    Err(ExposureError::Command {
        program,
        message: if message.is_empty() {
            format!("process exited with {}", output.status)
        } else {
            message
        },
    })
}

fn empty_configuration(value: &Value) -> bool {
    matches!(value, Value::Null) || value.as_object().is_some_and(serde_json::Map::is_empty)
}

fn tailscale_public_url(dns_name: &str, https_port: u16) -> Result<Url, ExposureError> {
    let host = dns_name.trim_end_matches('.');
    let value = if https_port == 443 {
        format!("https://{host}")
    } else {
        format!("https://{host}:{https_port}")
    };
    Url::parse(&value).map_err(|_| ExposureError::InvalidPublicUrl(value))
}

fn cloudflare_public_url(hostname: &str) -> Result<Url, ExposureError> {
    let value = if hostname.starts_with("https://") {
        hostname.to_string()
    } else {
        format!("https://{hostname}")
    };
    let url = Url::parse(&value).map_err(|_| ExposureError::InvalidPublicUrl(value.clone()))?;
    let valid_hostname = url
        .host_str()
        .is_some_and(|host| host.ends_with(".trycloudflare.com"));
    if url.scheme() != "https"
        || !valid_hostname
        || url.port().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ExposureError::InvalidPublicUrl(value));
    }
    Ok(url)
}

fn cloudflared_arguments(metrics_address: &str, target: &Url) -> Vec<String> {
    [
        "tunnel",
        "--no-autoupdate",
        "--metrics",
        metrics_address,
        "--protocol",
        "http2",
        "--url",
        target.as_str(),
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect()
}

fn format_logs(logs: &str) -> String {
    if logs.is_empty() {
        String::new()
    } else {
        format!("\ncloudflared diagnostics:\n{logs}")
    }
}

fn format_diagnostic(status: String, logs: &str) -> String {
    format!(" ({status}){}", format_logs(logs))
}

/// Whether `error` looks like cloudflared exited because the metrics port we picked for it was
/// taken by another process before it could bind it (a TOCTOU race between our port probe and
/// cloudflared's own bind). Only this specific, transient failure is worth retrying with a fresh
/// port; every other failure should surface immediately with its existing diagnostics.
fn is_metrics_bind_failure(error: &ExposureError) -> bool {
    match error {
        ExposureError::CloudflareExited(diagnostic) => {
            let diagnostic = diagnostic.to_lowercase();
            diagnostic.contains("address already in use") || diagnostic.contains("bind")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_an_empty_tailscale_configuration() {
        assert!(empty_configuration(&serde_json::json!({})));
        assert!(empty_configuration(&Value::Null));
        assert!(!empty_configuration(&serde_json::json!({"TCP":{"443":{}}})));
    }

    #[test]
    fn builds_the_tailscale_url() {
        assert_eq!(
            tailscale_public_url("mac.example.ts.net.", 443)
                .unwrap()
                .as_str(),
            "https://mac.example.ts.net/"
        );
        assert_eq!(
            tailscale_public_url("mac.example.ts.net.", 8443)
                .unwrap()
                .as_str(),
            "https://mac.example.ts.net:8443/"
        );
        assert_eq!(
            tailscale_public_url("mac.example.ts.net.", 8080)
                .unwrap()
                .as_str(),
            "https://mac.example.ts.net:8080/"
        );
    }

    #[test]
    fn accepts_only_cloudflare_quick_tunnel_urls() {
        assert_eq!(
            cloudflare_public_url("random-words.trycloudflare.com")
                .unwrap()
                .as_str(),
            "https://random-words.trycloudflare.com/"
        );
        assert!(cloudflare_public_url("http://random-words.trycloudflare.com").is_err());
        assert!(cloudflare_public_url("https://example.com").is_err());
        assert!(cloudflare_public_url("https://trycloudflare.com.evil.test").is_err());
    }

    #[test]
    fn cloudflare_quick_tunnel_uses_http2() {
        let target = Url::parse("http://127.0.0.1:49152").unwrap();
        assert_eq!(
            cloudflared_arguments("127.0.0.1:49153", &target),
            vec![
                "tunnel",
                "--no-autoupdate",
                "--metrics",
                "127.0.0.1:49153",
                "--protocol",
                "http2",
                "--url",
                "http://127.0.0.1:49152/",
            ]
        );
    }

    #[test]
    fn parses_the_tailscale_status_field_names() {
        let status: TailscaleStatus = serde_json::from_value(serde_json::json!({
            "BackendState": "Running",
            "Self": {"DNSName": "mac.example.ts.net."}
        }))
        .unwrap();
        assert_eq!(status.backend_state, "Running");
        assert_eq!(
            status.self_node.unwrap().dns_name.as_deref(),
            Some("mac.example.ts.net.")
        );
    }

    #[test]
    fn validates_tailscale_port_rules_by_mode() {
        assert!(validate_tailscale_port(TailscaleMode::Serve, 1).is_ok());
        assert!(validate_tailscale_port(TailscaleMode::Serve, u16::MAX).is_ok());
        assert!(validate_tailscale_port(TailscaleMode::Serve, 0).is_err());
        assert!(validate_tailscale_port(TailscaleMode::Funnel, 443).is_ok());
        assert!(validate_tailscale_port(TailscaleMode::Funnel, 8443).is_ok());
        assert!(validate_tailscale_port(TailscaleMode::Funnel, 10000).is_ok());
        assert!(validate_tailscale_port(TailscaleMode::Funnel, 8080).is_err());
        assert!(validate_tailscale_port(TailscaleMode::Funnel, 0).is_err());
    }

    #[test]
    fn provider_availability_uses_each_provider_override_independently() {
        let temporary = tempfile::tempdir().unwrap();
        let tailscale = temporary.path().join("tailscale");
        let cloudflared = temporary.path().join("cloudflared");
        std::fs::write(&tailscale, b"").unwrap();
        std::fs::write(&cloudflared, b"").unwrap();

        assert!(provider_binary_available(
            ExposureProvider::TailscaleServe,
            Some(&tailscale),
            None,
        ));
        assert!(provider_binary_available(
            ExposureProvider::TailscaleFunnel,
            Some(&tailscale),
            Some(&temporary.path().join("missing-cloudflared")),
        ));
        assert!(provider_binary_available(
            ExposureProvider::Cloudflare,
            Some(&temporary.path().join("missing-tailscale")),
            Some(&cloudflared),
        ));
        assert!(!provider_binary_available(
            ExposureProvider::Cloudflare,
            None,
            Some(&temporary.path().join("missing-cloudflared")),
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn auto_preflight_checks_both_tailscale_modes_before_starting() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        let log = temporary.path().join("commands.log");
        write_fake_tailscale(
            &binary,
            &log,
            r#"{"BackendState":"Running","Self":{"DNSName":"mac.example.ts.net."}}"#,
            "{}",
        );

        ExposureSession::check_tailscale_for_auto(Some(&binary))
            .await
            .unwrap();
        let commands = std::fs::read_to_string(log).unwrap();
        assert!(commands.contains("status --json"));
        assert!(commands.contains("serve status --json"));
        assert!(commands.contains("funnel status --json"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owns_the_tailscale_funnel_foreground_process() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        let log = temporary.path().join("commands.log");
        write_fake_tailscale(
            &binary,
            &log,
            r#"{"BackendState":"Running","Self":{"DNSName":"mac.example.ts.net."}}"#,
            "{}",
        );

        let target = Url::parse("http://127.0.0.1:49152").unwrap();
        let mut session = ExposureSession::start(
            ExposureProvider::TailscaleFunnel,
            &target,
            Some(&binary),
            None,
            443,
        )
        .await
        .unwrap();
        session.stop().await.unwrap();

        let commands = std::fs::read_to_string(log).unwrap();
        assert!(commands.contains("status --json"));
        assert!(commands.matches("funnel status --json").count() >= 2);
        assert!(commands.contains("funnel --yes --https=443 http://127.0.0.1:49152/"));
        assert!(!commands.contains(" off"));
        assert!(!commands.contains("--bg"));
        assert_fake_child_exited(&binary);
        assert!(!commands.contains("reset"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owns_the_tailscale_serve_foreground_process() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        let log = temporary.path().join("commands.log");
        write_fake_tailscale(
            &binary,
            &log,
            r#"{"BackendState":"Running","Self":{"DNSName":"mac.example.ts.net."}}"#,
            "{}",
        );

        let target = Url::parse("http://127.0.0.1:49152").unwrap();
        let mut session = ExposureSession::start(
            ExposureProvider::TailscaleServe,
            &target,
            Some(&binary),
            None,
            8080,
        )
        .await
        .unwrap();
        assert_eq!(session.provider(), ExposureProvider::TailscaleServe);
        assert_eq!(
            session.public_base_url().as_str(),
            "https://mac.example.ts.net:8080/"
        );
        session.stop().await.unwrap();

        let commands = std::fs::read_to_string(log).unwrap();
        assert!(commands.matches("serve status --json").count() >= 2);
        assert!(commands.contains("serve --yes --https=8080 http://127.0.0.1:49152/"));
        assert!(!commands.contains(" off"));
        assert!(!commands.contains("--bg"));
        assert_fake_child_exited(&binary);
        assert!(!commands.contains("funnel"));
        assert!(!commands.contains("reset"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn warns_when_local_tailscale_dns_is_disabled_for_serve() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        write_fake_tailscale(
            &binary,
            &temporary.path().join("commands.log"),
            r#"{"BackendState":"Running","Self":{"DNSName":"mac.example.ts.net."}}"#,
            "{}",
        );
        write_fake_dns_status(
            &binary,
            r#"{"TailscaleDNS":false,"CurrentTailnet":{"MagicDNSEnabled":true,"SelfDNSName":"mac.example.ts.net."}}"#,
        );

        let target = Url::parse("http://127.0.0.1:49152").unwrap();
        let mut session = ExposureSession::start(
            ExposureProvider::TailscaleServe,
            &target,
            Some(&binary),
            None,
            443,
        )
        .await
        .unwrap();
        assert_eq!(session.warnings().len(), 1);
        assert!(session.warnings()[0].contains("tailscale set --accept-dns=true"));
        assert!(
            session.warnings()[0]
                .to_ascii_lowercase()
                .contains("receiving devices")
        );
        session.stop().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn warns_when_tailnet_magicdns_is_disabled_for_serve() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        write_fake_tailscale(
            &binary,
            &temporary.path().join("commands.log"),
            r#"{"BackendState":"Running","Self":{"DNSName":"mac.example.ts.net."}}"#,
            "{}",
        );
        write_fake_dns_status(
            &binary,
            r#"{"TailscaleDNS":true,"CurrentTailnet":{"MagicDNSEnabled":false,"SelfDNSName":"mac.example.ts.net."}}"#,
        );

        let target = Url::parse("http://127.0.0.1:49152").unwrap();
        let mut session = ExposureSession::start(
            ExposureProvider::TailscaleServe,
            &target,
            Some(&binary),
            None,
            443,
        )
        .await
        .unwrap();
        assert_eq!(session.warnings().len(), 1);
        assert!(session.warnings()[0].contains("MagicDNS is disabled"));
        assert!(
            session.warnings()[0]
                .to_ascii_lowercase()
                .contains("receiving devices")
        );
        session.stop().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn accepts_disabled_local_tailscale_dns_for_funnel_without_warning() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        write_fake_tailscale(
            &binary,
            &temporary.path().join("commands.log"),
            r#"{"BackendState":"Running","Self":{"DNSName":"mac.example.ts.net."}}"#,
            "{}",
        );
        write_fake_dns_status(
            &binary,
            r#"{"TailscaleDNS":false,"CurrentTailnet":{"MagicDNSEnabled":true,"SelfDNSName":"mac.example.ts.net."}}"#,
        );

        let target = Url::parse("http://127.0.0.1:49152").unwrap();
        let mut session = ExposureSession::start(
            ExposureProvider::TailscaleFunnel,
            &target,
            Some(&binary),
            None,
            443,
        )
        .await
        .unwrap();
        assert!(session.warnings().is_empty());
        session.stop().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_funnel_when_tailnet_magicdns_is_disabled_before_spawning() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        write_fake_tailscale(
            &binary,
            &temporary.path().join("commands.log"),
            r#"{"BackendState":"Running","Self":{"DNSName":"mac.example.ts.net."}}"#,
            "{}",
        );
        write_fake_dns_status(
            &binary,
            r#"{"TailscaleDNS":true,"CurrentTailnet":{"MagicDNSEnabled":false,"SelfDNSName":"mac.example.ts.net."}}"#,
        );

        let target = Url::parse("http://127.0.0.1:49152").unwrap();
        let error = ExposureSession::start(
            ExposureProvider::TailscaleFunnel,
            &target,
            Some(&binary),
            None,
            443,
        )
        .await
        .err()
        .expect("MagicDNS-disabled Funnel should be rejected before spawning");
        assert!(
            error
                .to_string()
                .contains("enable MagicDNS in the Tailscale admin console DNS settings")
        );
        assert!(!binary.with_extension("pid").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_malformed_or_failed_tailscale_dns_diagnostics_before_spawning() {
        for failure in ["malformed", "missing", "failed"] {
            let temporary = tempfile::tempdir().unwrap();
            let binary = temporary.path().join("tailscale");
            write_fake_tailscale(
                &binary,
                &temporary.path().join("commands.log"),
                r#"{"BackendState":"Running","Self":{"DNSName":"mac.example.ts.net."}}"#,
                "{}",
            );
            if failure == "malformed" {
                write_fake_dns_status(&binary, "not-json");
            } else if failure == "missing" {
                write_fake_dns_status(&binary, r#"{"TailscaleDNS":true,"CurrentTailnet":{}}"#);
            } else {
                mark_fake_dns_status_failed(&binary);
            }

            let target = Url::parse("http://127.0.0.1:49152").unwrap();
            let error = ExposureSession::start(
                ExposureProvider::TailscaleServe,
                &target,
                Some(&binary),
                None,
                443,
            )
            .await
            .err()
            .expect("invalid DNS diagnostics should be reported before spawning");
            assert!(error.to_string().contains("tailscale dns status --json"));
            assert!(!binary.with_extension("pid").exists());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn times_out_tailscale_dns_diagnostics_before_spawning() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        write_fake_tailscale(
            &binary,
            &temporary.path().join("commands.log"),
            r#"{"BackendState":"Running","Self":{"DNSName":"mac.example.ts.net."}}"#,
            "{}",
        );
        mark_fake_dns_status_slow(&binary);

        let target = Url::parse("http://127.0.0.1:49152").unwrap();
        let started = std::time::Instant::now();
        let error = ExposureSession::start(
            ExposureProvider::TailscaleServe,
            &target,
            Some(&binary),
            None,
            443,
        )
        .await
        .err()
        .expect("stalled DNS diagnostics should be rejected");
        assert!(started.elapsed() < Duration::from_secs(8));
        assert!(error.to_string().contains("timed out after 5 seconds"));
        assert!(!binary.with_extension("pid").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_existing_configuration_without_starting_or_resetting_it() {
        for (provider, mode) in [
            (ExposureProvider::TailscaleServe, "Serve"),
            (ExposureProvider::TailscaleFunnel, "Funnel"),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let binary = temporary.path().join("tailscale");
            let log = temporary.path().join("commands.log");
            write_fake_tailscale(
                &binary,
                &log,
                r#"{"BackendState":"Running","Self":{"DNSName":"mac.example.ts.net."}}"#,
                r#"{"TCP":{"443":{}}}"#,
            );
            let target = Url::parse("http://127.0.0.1:49152").unwrap();
            let error = ExposureSession::start(provider, &target, Some(&binary), None, 443)
                .await
                .err()
                .expect("existing configuration should be refused");
            assert_eq!(
                error.to_string(),
                format!(
                    "an existing Tailscale {mode} configuration is active; refusing to replace it"
                )
            );
            let commands = std::fs::read_to_string(log).unwrap();
            assert!(commands.contains(&format!("{} status --json", mode.to_lowercase())));
            assert!(!commands.contains(" --yes "));
            assert!(!commands.contains("reset"));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_a_stopped_tailscale_backend() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        let log = temporary.path().join("commands.log");
        write_fake_tailscale(
            &binary,
            &log,
            r#"{"BackendState":"Stopped","Self":{"DNSName":"mac.example.ts.net."}}"#,
            "{}",
        );
        let target = Url::parse("http://127.0.0.1:49152").unwrap();
        let error = ExposureSession::start(
            ExposureProvider::TailscaleServe,
            &target,
            Some(&binary),
            None,
            443,
        )
        .await
        .err()
        .expect("stopped backend should be rejected");
        assert_eq!(
            error.to_string(),
            "Tailscale is not ready: backend state is Stopped"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_missing_tailscale_dns_name() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        let log = temporary.path().join("commands.log");
        write_fake_tailscale(
            &binary,
            &log,
            r#"{"BackendState":"Running","Self":{}}"#,
            "{}",
        );
        let target = Url::parse("http://127.0.0.1:49152").unwrap();
        let error = ExposureSession::start(
            ExposureProvider::TailscaleFunnel,
            &target,
            Some(&binary),
            None,
            443,
        )
        .await
        .err()
        .expect("missing DNS name should be rejected");
        assert_eq!(
            error.to_string(),
            "Tailscale is not ready: the current node has no MagicDNS name"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_tailscale_cli_failures() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        let log = temporary.path().join("commands.log");
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
if [ "$1" = "status" ]; then
  printf '%s\n' 'tailscaled is unavailable' >&2
  exit 1
fi
"#,
            log.display()
        );
        std::fs::write(&binary, script).unwrap();
        let mut permissions = std::fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&binary, permissions).unwrap();

        let target = Url::parse("http://127.0.0.1:49152").unwrap();
        let error = ExposureSession::start(
            ExposureProvider::TailscaleServe,
            &target,
            Some(&binary),
            None,
            443,
        )
        .await
        .err()
        .expect("CLI failure should be returned");
        assert_eq!(
            error.to_string(),
            "Tailscale failed: tailscaled is unavailable"
        );
    }

    #[cfg(unix)]
    fn write_fake_tailscale(
        binary: &std::path::Path,
        log: &std::path::Path,
        status_json: &str,
        configuration_json: &str,
    ) {
        use std::os::unix::fs::PermissionsExt;

        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
if [ "$1" = "status" ]; then
  printf '%s\n' '{}'
elif [ "$1" = "dns" ] && [ "$2" = "status" ]; then
  if [ -f "$0.dns-fail" ]; then
    printf '%s\n' 'dns status is unavailable' >&2
    exit 1
  elif [ -f "$0.dns-slow" ]; then
    sleep 6
    printf '%s\n' '{{"TailscaleDNS":true,"CurrentTailnet":{{"MagicDNSEnabled":true,"SelfDNSName":"mac.example.ts.net."}}}}'
  elif [ -f "$0.dns" ]; then
    cat "$0.dns"
  else
    printf '%s\n' '{{"TailscaleDNS":true,"CurrentTailnet":{{"MagicDNSEnabled":true,"SelfDNSName":"mac.example.ts.net."}}}}'
  fi
elif [ "$2" = "status" ]; then
  if [ -f "$0.config" ]; then
    cat "$0.config"
  else
    printf '%s\n' '{}'
  fi
else
  printf '%s\n' "$$" > "$0.pid"
  port=${{3#--https=}}
  public=false
  if [ "$1" = "funnel" ]; then public=true; fi
  printf '{{"Foreground":{{"test-session":{{"TCP":{{"%s":{{"HTTPS":true}}}},"Web":{{"mac.example.ts.net:%s":{{"Handlers":{{"/":{{"Proxy":"%s"}}}}}}}},"AllowFunnel":{{"mac.example.ts.net:%s":%s}}}}}}}}\n' "$port" "$port" "$4" "$port" "$public" > "$0.config"
  printf '%s\n' 'Available; foreground session is running'
  exec sleep 60
fi
"#,
            log.display(),
            status_json,
            configuration_json
        );
        std::fs::write(binary, script).unwrap();
        let mut permissions = std::fs::metadata(binary).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(binary, permissions).unwrap();
    }

    #[cfg(unix)]
    fn write_fake_dns_status(binary: &Path, status_json: &str) {
        std::fs::write(binary.with_extension("dns"), status_json).unwrap();
    }

    #[cfg(unix)]
    fn mark_fake_dns_status_failed(binary: &Path) {
        std::fs::write(binary.with_extension("dns-fail"), b"").unwrap();
    }

    #[cfg(unix)]
    fn mark_fake_dns_status_slow(binary: &Path) {
        std::fs::write(binary.with_extension("dns-slow"), b"").unwrap();
    }

    #[cfg(unix)]
    fn assert_fake_child_exited(binary: &Path) {
        let pid = std::fs::read_to_string(binary.with_extension("pid")).unwrap();
        assert!(
            !std::process::Command::new("kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn readiness_requires_exact_foreground_https_proxy_and_scope() {
        let target = Url::parse("http://127.0.0.1:49152/").unwrap();
        let mut config = serde_json::json!({"Foreground":{"session":{
            "TCP":{"443":{"HTTPS":true}},
            "Web":{"mac.example.ts.net:443":{"Handlers":{"/":{"Proxy":target.as_str()}}}}
        }}});
        assert!(tailscale_configuration_ready(
            &config,
            TailscaleMode::Serve,
            "mac.example.ts.net.",
            443,
            &target
        ));
        assert!(!tailscale_configuration_ready(
            &config,
            TailscaleMode::Funnel,
            "mac.example.ts.net.",
            443,
            &target
        ));
        config["Foreground"]["session"]["AllowFunnel"] =
            serde_json::json!({"mac.example.ts.net:443":true});
        assert!(tailscale_configuration_ready(
            &config,
            TailscaleMode::Funnel,
            "mac.example.ts.net.",
            443,
            &target
        ));
        assert!(!tailscale_configuration_ready(
            &config,
            TailscaleMode::Serve,
            "mac.example.ts.net.",
            443,
            &target
        ));
        let other_target = Url::parse("http://127.0.0.1:49153/").unwrap();
        assert!(!tailscale_configuration_ready(
            &config,
            TailscaleMode::Funnel,
            "mac.example.ts.net.",
            443,
            &other_target
        ));
        assert!(!tailscale_configuration_ready(
            &config,
            TailscaleMode::Funnel,
            "mac.example.ts.net.",
            8443,
            &target
        ));
        assert!(!tailscale_configuration_ready(
            &config["Foreground"]["session"],
            TailscaleMode::Funnel,
            "mac.example.ts.net.",
            443,
            &target
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_foreground_exit_and_reaps_the_child() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        write_fake_tailscale(
            &binary,
            &temporary.path().join("commands.log"),
            r#"{"BackendState":"Running","Self":{"DNSName":"mac.example.ts.net."}}"#,
            "{}",
        );
        let target = Url::parse("http://127.0.0.1:49152/").unwrap();
        let mut session = ExposureSession::start(
            ExposureProvider::TailscaleServe,
            &target,
            Some(&binary),
            None,
            443,
        )
        .await
        .unwrap();
        let ExposureSessionInner::Tailscale(inner) = &mut session.inner else {
            panic!("Tailscale")
        };
        inner.child.start_kill().unwrap();
        let error = session.wait_for_exit().await.unwrap_err();
        assert!(matches!(error, ExposureError::TailscaleExited(_)));
        session.stop().await.unwrap();
        assert_fake_child_exited(&binary);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_cancellation_kills_the_child_waiting_for_setup() {
        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        write_fake_tailscale(
            &binary,
            &temporary.path().join("commands.log"),
            r#"{"BackendState":"Running","Self":{"DNSName":"mac.example.ts.net."}}"#,
            "{}",
        );
        // No foreground configuration appears while waiting for first-use setup.
        let script = std::fs::read_to_string(&binary)
            .unwrap()
            .replace("if [ -f \"$0.config\" ]; then", "if false; then");
        std::fs::write(&binary, script).unwrap();
        let target = Url::parse("http://127.0.0.1:49152/").unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(800),
            ExposureSession::start(
                ExposureProvider::TailscaleFunnel,
                &target,
                Some(&binary),
                None,
                443,
            ),
        )
        .await;
        assert!(result.is_err(), "should still be waiting for setup");
        // Tokio reaps kill_on_drop children asynchronously.
        sleep(Duration::from_millis(100)).await;
        assert_fake_child_exited(&binary);
    }

    #[tokio::test]
    async fn setup_diagnostics_are_consumed_before_the_stream_closes() {
        use tokio::io::AsyncWriteExt;
        let (mut writer, reader) = tokio::io::duplex(256);
        let logs = CapturedLogs::default();
        let task = tailscale_log_task(reader, logs.clone());
        writer
            .write_all(b"To enable, visit: https://login.tailscale.com/f/funnel\n")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !logs
                .snapshot()
                .await
                .contains("https://login.tailscale.com/f/funnel")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("setup URL must be read while CLI is still waiting");
        assert!(!task.is_finished());
        drop(writer);
        task.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_foreground_start_reports_diagnostics_for_both_modes() {
        for provider in [
            ExposureProvider::TailscaleServe,
            ExposureProvider::TailscaleFunnel,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            let binary = temporary.path().join("tailscale");
            write_fake_tailscale(
                &binary,
                &temporary.path().join("commands.log"),
                r#"{"BackendState":"Running","Self":{"DNSName":"mac.example.ts.net."}}"#,
                "{}",
            );
            let script = std::fs::read_to_string(&binary)
                .unwrap()
                .replace("if [ -f \"$0.config\" ]; then", "if false; then")
                .replace("exec sleep 60", "echo 'HTTPS setup denied' >&2\nexit 7");
            std::fs::write(&binary, script).unwrap();
            let target = Url::parse("http://127.0.0.1:49152/").unwrap();
            let error = ExposureSession::start(provider, &target, Some(&binary), None, 443)
                .await
                .err()
                .expect("failed child must not report ready");
            assert!(error.to_string().contains("HTTPS setup denied"));
            assert_fake_child_exited(&binary);
        }
    }

    #[test]
    fn cli_not_found_message_gives_actionable_cloudflared_install_steps() {
        let error = discover_binary(
            "cloudflared",
            "cloudflared-binary-that-should-not-exist-anywhere",
            None,
            &[],
            CLOUDFLARED_INSTALL_HINT,
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("brew install cloudflared"));
        assert!(message.contains("https://github.com/cloudflare/cloudflared/releases"));
        assert!(message.contains("--cloudflared-bin"));
    }

    #[test]
    fn cli_not_found_message_points_tailscale_users_at_the_cloudflare_alternative() {
        let error = discover_binary(
            "Tailscale",
            "tailscale-binary-that-should-not-exist-anywhere",
            None,
            &[],
            TAILSCALE_INSTALL_HINT,
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("--provider cloudflare"));
        assert!(message.contains("--tailscale-bin"));
    }

    #[test]
    fn detects_metrics_bind_failures_case_insensitively() {
        let bind_failure = ExposureError::CloudflareExited(
            " (exit status: 1)\ncloudflared diagnostics:\nERROR: Address Already In Use".into(),
        );
        assert!(is_metrics_bind_failure(&bind_failure));

        let lowercase_bind_wording = ExposureError::CloudflareExited(
            " (exit status: 1)\ncloudflared diagnostics:\nfailed to bind metrics server".into(),
        );
        assert!(is_metrics_bind_failure(&lowercase_bind_wording));

        let unrelated_exit = ExposureError::CloudflareExited(
            " (exit status: 1)\ncloudflared diagnostics:\nERROR: connection refused".into(),
        );
        assert!(!is_metrics_bind_failure(&unrelated_exit));

        let unrelated_variant = ExposureError::TailscaleNotReady("backend state is Stopped".into());
        assert!(!is_metrics_bind_failure(&unrelated_variant));
    }
}
