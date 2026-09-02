use std::collections::VecDeque;
use std::future::pending;
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
use tokio::time::{Instant, sleep};
use url::Url;

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

Note: Tailscale Funnel requires a Tailscale account and a tailnet with Funnel enabled, unlike
the Cloudflare path above. If you'd rather not create an account, pass --provider cloudflare
to use the Cloudflare Quick Tunnel instead.
If Tailscale is installed somewhere else, pass --tailscale-bin /path/to/tailscale.
Already checked: $PATH, /Applications/Tailscale.app/Contents/MacOS/Tailscale.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExposureProvider {
    Tailscale,
    Cloudflare,
}

impl ExposureProvider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Tailscale => "Tailscale Funnel",
            Self::Cloudflare => "Cloudflare Quick Tunnel",
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
    #[error("an existing Tailscale Funnel configuration is active; refusing to replace it")]
    ExistingTailscaleConfiguration,
    #[error("{program} failed: {message}")]
    Command {
        program: &'static str,
        message: String,
    },
    #[error("tunnel provider reported an invalid public URL: {0}")]
    InvalidPublicUrl(String),
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
    inner: ExposureSessionInner,
}

enum ExposureSessionInner {
    Tailscale(TailscaleSession),
    Cloudflare(CloudflareSession),
}

#[derive(Debug)]
struct TailscaleSession {
    binary: PathBuf,
    https_port: u16,
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
    dns_name: String,
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
        match provider {
            ExposureProvider::Tailscale => {
                start_tailscale(target, tailscale_binary, tailscale_https_port).await
            }
            ExposureProvider::Cloudflare => start_cloudflare(target, cloudflared_binary).await,
        }
    }

    pub fn provider(&self) -> ExposureProvider {
        self.provider
    }

    pub fn public_base_url(&self) -> &Url {
        &self.public_base_url
    }

    pub async fn wait_for_exit(&mut self) -> Result<(), ExposureError> {
        match &mut self.inner {
            ExposureSessionInner::Tailscale(_) => pending().await,
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
    async fn stop(&self) -> Result<(), ExposureError> {
        let port_flag = format!("--https={}", self.https_port);
        run_checked(
            "Tailscale",
            &self.binary,
            &["funnel", port_flag.as_str(), "off"],
        )
        .await
        .map(|_| ())
    }
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
    target: &Url,
    binary_override: Option<&Path>,
    https_port: u16,
) -> Result<ExposureSession, ExposureError> {
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
        .map(|node| node.dns_name)
        .filter(|name| !name.trim_matches('.').is_empty())
        .ok_or_else(|| {
            ExposureError::TailscaleNotReady("the current node has no MagicDNS name".into())
        })?;
    ensure_empty_tailscale_configuration(&binary).await?;

    let public_base_url = tailscale_public_url(&dns_name, https_port)?;
    let port_flag = format!("--https={https_port}");
    run_checked(
        "Tailscale",
        &binary,
        &[
            "funnel",
            "--yes",
            "--bg",
            port_flag.as_str(),
            target.as_str(),
        ],
    )
    .await?;
    Ok(ExposureSession {
        provider: ExposureProvider::Tailscale,
        public_base_url,
        inner: ExposureSessionInner::Tailscale(TailscaleSession { binary, https_port }),
    })
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

async fn ensure_empty_tailscale_configuration(binary: &Path) -> Result<(), ExposureError> {
    let current: Value = command_json(binary, &["funnel", "status", "--json"]).await?;
    if empty_configuration(&current) {
        Ok(())
    } else {
        Err(ExposureError::ExistingTailscaleConfiguration)
    }
}

fn discover_binary(
    display_name: &'static str,
    executable_name: &str,
    binary_override: Option<&Path>,
    standard_paths: &[&str],
    install_hint: &'static str,
) -> Result<PathBuf, ExposureError> {
    if let Some(path) = binary_override {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join(executable_name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    standard_paths
        .iter()
        .map(PathBuf::from)
        .find(|candidate| candidate.is_file())
        .ok_or(ExposureError::CliNotFound {
            display_name,
            install_hint,
        })
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
    let output = Command::new(binary).args(arguments).output().await?;
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
    fn builds_the_tailscale_funnel_url() {
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
        assert_eq!(status.self_node.unwrap().dns_name, "mac.example.ts.net.");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owns_only_the_tailscale_port_it_started() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let binary = temporary.path().join("tailscale");
        let log = temporary.path().join("commands.log");
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> '{}'
if [ "$1" = "status" ]; then
  printf '%s\n' '{{"BackendState":"Running","Self":{{"DNSName":"mac.example.ts.net."}}}}'
elif [ "$1" = "funnel" ] && [ "$2" = "status" ]; then
  printf '%s\n' '{{}}'
fi
"#,
            log.display()
        );
        std::fs::write(&binary, script).unwrap();
        let mut permissions = std::fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&binary, permissions).unwrap();

        let target = Url::parse("http://127.0.0.1:49152").unwrap();
        let mut session = ExposureSession::start(
            ExposureProvider::Tailscale,
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
        assert_eq!(commands.matches("funnel status --json").count(), 1);
        assert!(commands.contains("funnel --yes --bg --https=443 http://127.0.0.1:49152/"));
        assert!(commands.contains("funnel --https=443 off"));
        assert!(!commands.contains("reset"));
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
