use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use futures_util::stream::{FuturesUnordered, StreamExt};
use remote_installer::apk::ApkToolchain;
use remote_installer::artifact_input::{self, PreparationStage, SigningPolicy};
use remote_installer::exposure::{ExposureProvider, ExposureSession, provider_binary_available};
use remote_installer::http::{self, HttpState};
use remote_installer::model::{Artifact, Availability, PlatformMetadata};
use remote_installer::service::{ShareConfig, ShareService};
use tracing_subscriber::EnvFilter;
use url::Url;

const TUNNEL_STARTUP_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const BUILD_PREPARATION_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(
    name = "remote-installer",
    version,
    about = "Standalone iOS and Android app distribution over a temporary HTTPS tunnel"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Share one IPA, signed iOS .app, or signed standalone APK.
    #[command(after_help = SHARE_EXAMPLES)]
    Share(ShareArgs),
}

const SHARE_EXAMPLES: &str = "\
Examples:
  # Share until you press Ctrl-C
  remote-installer share MyApp.ipa

  # Close the link automatically after five minutes
  remote-installer share MyApp.ipa --timeout 300

  # Let exactly one person install, then stop
  remote-installer share MyApp.ipa --max-downloads 1

  # Keep the install page private to your tailnet
  remote-installer share MyApp.ipa --provider tailscale-serve

  # Share a public link through Tailscale Funnel
  remote-installer share MyApp.ipa --provider tailscale-funnel

  # Share a signed standalone Android APK
  remote-installer share MyApp.apk
";

#[derive(Debug, Args)]
struct ShareArgs {
    /// Development/ad-hoc IPA, iphoneos .app, or signed standalone APK.
    artifact: PathBuf,
    /// Stop sharing and exit after this many seconds.
    #[arg(
        long,
        value_name = "SECONDS",
        value_parser = parse_timeout_seconds,
        conflicts_with = "expire_after"
    )]
    timeout: Option<u64>,
    /// Same as --timeout, but with a unit: `90s`, `30m`, `2h`, `7d`.
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    expire_after: Option<Duration>,
    /// Stop sharing after this many OTA download attempts.
    #[arg(long, value_name = "COUNT")]
    max_downloads: Option<u64>,
    /// Tunnel provider (`tailscale` is an alias for `tailscale-funnel`).
    /// Omit this option to detect and start every installed provider.
    #[arg(long, value_enum, default_value_t = ShareProvider::Auto)]
    provider: ShareProvider,
    /// Do not print the install link as a terminal QR code.
    #[arg(long)]
    no_qr: bool,
    /// Share an IPA even when its iOS signing evidence is unusable. This does
    /// not control APK checks.
    #[arg(long)]
    allow_unsigned: bool,
    /// HTTPS port used by Tailscale Serve or an explicitly selected Funnel.
    /// Auto mode picks another supported Funnel port; `--funnel-port` remains
    /// a visible compatibility alias.
    #[arg(
        long = "https-port",
        visible_alias = "funnel-port",
        value_name = "PORT",
        default_value_t = 443,
        value_parser = parse_https_port
    )]
    https_port: u16,
    /// Explicit path to the Tailscale CLI.
    #[arg(long, value_name = "PATH")]
    tailscale_bin: Option<PathBuf>,
    /// Explicit path to the cloudflared CLI.
    #[arg(long, value_name = "PATH")]
    cloudflared_bin: Option<PathBuf>,
    /// Explicit path to the Android SDK apkanalyzer tool.
    #[arg(long, value_name = "PATH")]
    apkanalyzer_bin: Option<PathBuf>,
    /// Explicit path to the Android SDK apksigner tool.
    #[arg(long, value_name = "PATH")]
    apksigner_bin: Option<PathBuf>,
    /// Loopback address for the temporary origin server.
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:0")]
    listen: SocketAddr,
}

impl ShareArgs {
    /// How long the share stays installable.
    ///
    /// `--timeout` is the seconds-only spelling of `--expire-after`; they set
    /// the same limit, and clap rejects passing both rather than making anyone
    /// guess which one wins.
    fn artifact_ttl(&self) -> Option<Duration> {
        self.expire_after
            .or_else(|| self.timeout.map(Duration::from_secs))
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ShareProvider {
    #[value(name = "auto")]
    Auto,
    #[value(name = "tailscale-serve")]
    TailscaleServe,
    #[value(name = "tailscale-funnel", alias = "tailscale")]
    TailscaleFunnel,
    Cloudflare,
}

impl From<ShareProvider> for ExposureProvider {
    fn from(value: ShareProvider) -> Self {
        match value {
            ShareProvider::Auto => {
                panic!("auto provider must be resolved before conversion")
            }
            ShareProvider::TailscaleServe => Self::TailscaleServe,
            ShareProvider::TailscaleFunnel => Self::TailscaleFunnel,
            ShareProvider::Cloudflare => Self::Cloudflare,
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    let result = match Cli::parse().command {
        Command::Share(args) => share(args).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report(error.as_ref());
            ExitCode::FAILURE
        }
    }
}

/// Print a failure using `Display`, not `Debug`.
///
/// Returning `Result` from `main` would format with `Debug`, which turns a
/// multi-line remedy — such as the cloudflared install instructions — into one
/// line of escaped `\n`s, exactly when the user most needs to read it.
fn report(error: &(dyn std::error::Error + 'static)) {
    eprintln!("Error: {error}");
    let mut source = error.source();
    while let Some(cause) = source {
        eprintln!("  caused by: {cause}");
        source = cause.source();
    }
}

async fn share(args: ShareArgs) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !args.listen.ip().is_loopback() {
        return Err("--listen must be a loopback address when using share".into());
    }
    match args.provider {
        ShareProvider::Auto => {}
        ShareProvider::TailscaleServe | ShareProvider::TailscaleFunnel => {
            if args.cloudflared_bin.is_some() {
                return Err("--cloudflared-bin requires --provider cloudflare".into());
            }
        }
        ShareProvider::Cloudflare => {
            if args.tailscale_bin.is_some() {
                return Err(
                    "--tailscale-bin requires --provider tailscale-serve or tailscale-funnel"
                        .into(),
                );
            }
        }
    }
    if matches!(args.provider, ShareProvider::TailscaleFunnel) {
        validate_funnel_port(args.https_port)?;
    }

    let is_apk = args
        .artifact
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("apk"));
    if !is_apk && (args.apkanalyzer_bin.is_some() || args.apksigner_bin.is_some()) {
        return Err("--apkanalyzer-bin and --apksigner-bin require an APK artifact".into());
    }
    let apk_toolchain = is_apk
        .then(|| {
            ApkToolchain::discover(
                args.apkanalyzer_bin.as_deref(),
                args.apksigner_bin.as_deref(),
            )
        })
        .transpose()?;
    eprintln!("Preparing build: {}...", args.artifact.display());
    let temporary = tempfile::tempdir()?;
    let source = args.artifact.clone();
    let input_staging = temporary.path().join("input");
    let signing_policy = if args.allow_unsigned {
        SigningPolicy::Trusted
    } else {
        SigningPolicy::Required
    };
    // Validation runs before the tunnel opens, so a build that cannot install
    // fails without ever having been exposed publicly.
    let preparation_started_at = std::time::Instant::now();
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut preparation = tokio::task::spawn_blocking(move || {
        artifact_input::prepare_with_progress(
            &source,
            None,
            &input_staging,
            signing_policy,
            apk_toolchain.as_ref(),
            |stage| {
                let _ = progress_tx.send(stage);
            },
        )
    });
    let mut current_stage = PreparationStage::InspectingInput;
    let mut stage_started_at = preparation_started_at;
    let mut preparation_progress = tokio::time::interval_at(
        tokio::time::Instant::now() + BUILD_PREPARATION_PROGRESS_INTERVAL,
        BUILD_PREPARATION_PROGRESS_INTERVAL,
    );
    let prepared = loop {
        tokio::select! {
            // Drain stage changes before completion so short operations are
            // also reported, even when the blocking task has already finished.
            biased;
            Some(stage) = progress_rx.recv() => {
                current_stage = stage;
                stage_started_at = std::time::Instant::now();
                eprintln!("  {}...", preparation_stage_label(stage));
            }
            result = &mut preparation => break result??,
            _ = preparation_progress.tick() => eprintln!(
                "  Still working: {} ({} elapsed)...",
                preparation_stage_label(current_stage),
                format_duration(stage_started_at.elapsed()),
            ),
        }
    };
    eprintln!(
        "Build prepared in {:.1}s.",
        preparation_started_at.elapsed().as_secs_f64(),
    );
    let validation_was_degraded = !prepared.warnings().is_empty();
    for warning in prepared.warnings() {
        eprintln!("Warning: {warning}");
    }
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    let local_address = listener.local_addr()?;
    let origin_url = Url::parse(&format!("http://{local_address}"))?;
    let validation_summary = if validation_was_degraded {
        "Basic build checks passed"
    } else {
        "Build validated"
    };
    eprintln!("{validation_summary}. Starting supported tunnel providers...");
    let mut exposures = start_exposures(&args, &origin_url).await?;
    for exposure in &exposures {
        for warning in exposure.warnings() {
            eprintln!("Warning: {}: {warning}", exposure.provider().name());
        }
    }
    eprintln!("Tunnel startup complete. Preparing install page...");
    let public_base_urls = exposures
        .iter()
        .map(|exposure| exposure.public_base_url().clone())
        .collect();
    let setup_result = ShareService::create_with_public_base_urls(
        temporary.path().join("artifacts"),
        public_base_urls,
        &prepared,
        ShareConfig {
            artifact_ttl: args.artifact_ttl(),
            max_downloads: args.max_downloads,
        },
    )
    .await;
    let service = match setup_result {
        Ok(service) => Arc::new(service),
        Err(error) => {
            stop_exposures(&mut exposures).await;
            return Err(error.into());
        }
    };
    let artifact = service.artifact().clone();
    let links = match provider_links(&service, &artifact, &exposures).await {
        Ok(links) => links,
        Err(error) => {
            stop_exposures(&mut exposures).await;
            return Err(error);
        }
    };
    print_share_banners(&artifact, &links, &args);

    let state = HttpState {
        service: Arc::clone(&service),
    };
    let origin_result = tokio::select! {
        result = http::run_listener(listener, state, wait_for_shutdown(Arc::clone(&service))) => result,
        result = wait_for_any_exposure_exit(&mut exposures) => result,
    };
    let cleanup_errors = stop_exposures(&mut exposures).await;
    for error in cleanup_errors {
        tracing::error!(%error, "failed to close a tunnel");
        if origin_result.is_ok() {
            return Err(error);
        }
    }
    origin_result
}

#[derive(Debug, Clone, Copy)]
struct ProviderPlan {
    provider: ExposureProvider,
    https_port: u16,
}

#[derive(Debug)]
struct ProviderLink {
    provider: ExposureProvider,
    install_page_url: String,
    install_action_url: String,
}

fn provider_plans(args: &ShareArgs) -> Vec<ProviderPlan> {
    match args.provider {
        ShareProvider::Auto => vec![
            ProviderPlan {
                provider: ExposureProvider::TailscaleServe,
                https_port: args.https_port,
            },
            ProviderPlan {
                provider: ExposureProvider::TailscaleFunnel,
                // Serve and Funnel cannot share a Tailscale HTTPS port. Keep
                // both available in auto mode by selecting another supported
                // Funnel port; explicit provider selection retains the exact
                // --https-port value the caller requested.
                https_port: auto_funnel_port(args.https_port),
            },
            ProviderPlan {
                provider: ExposureProvider::Cloudflare,
                https_port: args.https_port,
            },
        ],
        ShareProvider::TailscaleServe => vec![ProviderPlan {
            provider: ExposureProvider::TailscaleServe,
            https_port: args.https_port,
        }],
        ShareProvider::TailscaleFunnel => vec![ProviderPlan {
            provider: ExposureProvider::TailscaleFunnel,
            https_port: args.https_port,
        }],
        ShareProvider::Cloudflare => vec![ProviderPlan {
            provider: ExposureProvider::Cloudflare,
            https_port: args.https_port,
        }],
    }
}

fn auto_funnel_port(serve_port: u16) -> u16 {
    [443, 8443, 10000]
        .into_iter()
        .find(|port| *port != serve_port)
        .unwrap_or(8443)
}

async fn start_exposures(
    args: &ShareArgs,
    target: &Url,
) -> Result<Vec<ExposureSession>, Box<dyn std::error::Error + Send + Sync>> {
    let auto = matches!(args.provider, ShareProvider::Auto);
    let mut plans = provider_plans(args);
    let tailscale_available = provider_binary_available(
        ExposureProvider::TailscaleServe,
        args.tailscale_bin.as_deref(),
        args.cloudflared_bin.as_deref(),
    );
    let cloudflare_available = provider_binary_available(
        ExposureProvider::Cloudflare,
        args.tailscale_bin.as_deref(),
        args.cloudflared_bin.as_deref(),
    );
    let tailscale_ready = if auto && tailscale_available {
        match ExposureSession::check_tailscale_for_auto(args.tailscale_bin.as_deref()).await {
            Ok(()) => true,
            Err(error) => {
                eprintln!(
                    "Warning: Tailscale is unavailable; skipping Tailscale providers: {error}"
                );
                false
            }
        }
    } else {
        false
    };

    if auto {
        plans.retain(|plan| {
            let available = match plan.provider {
                ExposureProvider::TailscaleServe
                | ExposureProvider::TailscaleFunnel
                | ExposureProvider::Tailscale => tailscale_available && tailscale_ready,
                ExposureProvider::Cloudflare => cloudflare_available,
            };
            if !available {
                eprintln!(
                    "Warning: {} is unavailable or not ready; skipping provider.",
                    plan.provider.name()
                );
            }
            available
        });
    }
    if plans.is_empty() {
        return Err("no supported tunnel provider is available".into());
    }

    let mut pending = plans
        .iter()
        .map(|plan| plan.provider.name())
        .collect::<Vec<_>>();
    let mut starts = FuturesUnordered::new();
    for plan in plans {
        let provider = plan.provider;
        let tailscale_binary = args.tailscale_bin.as_deref();
        let cloudflared_binary = args.cloudflared_bin.as_deref();
        let use_shared_tailscale_preflight = auto
            && tailscale_ready
            && matches!(
                provider,
                ExposureProvider::TailscaleServe
                    | ExposureProvider::TailscaleFunnel
                    | ExposureProvider::Tailscale
            );
        starts.push(async move {
            let result = if use_shared_tailscale_preflight {
                ExposureSession::start_without_configuration_check(
                    provider,
                    target,
                    tailscale_binary,
                    cloudflared_binary,
                    plan.https_port,
                )
                .await
            } else {
                ExposureSession::start(
                    provider,
                    target,
                    tailscale_binary,
                    cloudflared_binary,
                    plan.https_port,
                )
                .await
            };
            (plan, result)
        });
    }

    let startup_started_at = std::time::Instant::now();
    let mut startup_progress = tokio::time::interval_at(
        tokio::time::Instant::now() + TUNNEL_STARTUP_PROGRESS_INTERVAL,
        TUNNEL_STARTUP_PROGRESS_INTERVAL,
    );
    let mut results = Vec::new();
    while !starts.is_empty() {
        tokio::select! {
            biased;
            Some((plan, result)) = starts.next() => {
                pending.retain(|name| *name != plan.provider.name());
                results.push((plan, result));
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\nInterrupted while starting tunnel providers.");
                return Err("sharing interrupted during tunnel startup".into());
            }
            _ = startup_progress.tick() => eprintln!(
                "Still waiting for {}... ({})",
                pending.join(", "),
                format_duration(startup_started_at.elapsed()),
            ),
        }
    }

    let mut exposures = Vec::new();
    for (plan, result) in results {
        match result {
            Ok(exposure) => exposures.push(exposure),
            Err(error) if auto => eprintln!(
                "Warning: {} could not start; skipping provider: {error}",
                plan.provider.name()
            ),
            Err(error) => {
                return Err(format!("{} could not start: {error}", plan.provider.name()).into());
            }
        }
    }
    if exposures.is_empty() {
        return Err("all supported tunnel providers failed to start".into());
    }
    exposures.sort_by_key(|exposure| provider_order(exposure.provider()));
    Ok(exposures)
}

fn provider_order(provider: ExposureProvider) -> u8 {
    match provider {
        ExposureProvider::TailscaleServe => 0,
        ExposureProvider::TailscaleFunnel | ExposureProvider::Tailscale => 1,
        ExposureProvider::Cloudflare => 2,
    }
}

async fn provider_links(
    service: &ShareService,
    artifact: &Artifact,
    exposures: &[ExposureSession],
) -> Result<Vec<ProviderLink>, Box<dyn std::error::Error + Send + Sync>> {
    let mut links = Vec::with_capacity(exposures.len());
    for exposure in exposures {
        let public_base_url = exposure.public_base_url();
        links.push(ProviderLink {
            provider: exposure.provider(),
            install_page_url: service.install_page_url_at(artifact, public_base_url),
            install_action_url: service
                .install_action_url_at(artifact, public_base_url)
                .await?,
        });
    }
    Ok(links)
}

async fn wait_for_any_exposure_exit(
    exposures: &mut [ExposureSession],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let names = exposures
        .iter()
        .map(|exposure| exposure.provider().name())
        .collect::<Vec<_>>();
    let mut waits = FuturesUnordered::new();
    for (index, exposure) in exposures.iter_mut().enumerate() {
        waits.push(async move { (index, exposure.wait_for_exit().await) });
    }
    let result = waits.next().await;
    drop(waits);
    match result {
        Some((_index, Ok(()))) => Ok(()),
        Some((index, Err(error))) => Err(format!("{} exited: {error}", names[index]).into()),
        None => Ok(()),
    }
}

async fn stop_exposures(
    exposures: &mut [ExposureSession],
) -> Vec<Box<dyn std::error::Error + Send + Sync>> {
    let mut errors = Vec::new();
    for exposure in exposures {
        if let Err(error) = exposure.stop().await {
            errors.push(Box::new(error) as Box<dyn std::error::Error + Send + Sync>);
        }
    }
    errors
}

fn preparation_stage_label(stage: PreparationStage) -> &'static str {
    match stage {
        PreparationStage::InspectingInput => "Checking build input",
        PreparationStage::CheckingCodeSignature => "Verifying code signature",
        PreparationStage::CheckingDeviceArchitecture => "Checking device architecture",
        PreparationStage::CheckingProvisioningProfile => "Checking iOS signing and provisioning",
        PreparationStage::CopyingApp => "Copying app bundle",
        PreparationStage::PackagingIpa => "Packaging app as IPA",
        PreparationStage::InspectingPackage => "Inspecting package and calculating SHA-256",
    }
}

/// Resolve when the share should stop serving: Ctrl-C, the configured expiry,
/// or the last allowed download completing successfully.
///
/// Returning here starts a graceful drain that can take up to two minutes, so
/// a further Ctrl-C exits immediately instead. Without that escape the only
/// way out of a slow drain is SIGKILL, because tokio keeps consuming SIGINT
/// once its handler is installed.
async fn wait_for_shutdown(service: Arc<ShareService>) {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => println!("\nStopping..."),
        reason = service.wait_until_unavailable() => {
            let cause = match reason {
                Availability::Expired => "Share expired",
                Availability::LimitReached => "Download limit reached",
                Availability::Installable => "Share ended",
            };
            println!("\n{cause} — closing the tunnel.");
        }
    }
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nInterrupted again — exiting without finishing in-flight downloads.");
            std::process::exit(130);
        }
    });
}

fn print_share_banners(artifact: &Artifact, links: &[ProviderLink], args: &ShareArgs) {
    println!("App: {}", artifact.title());
    match &artifact.platform_metadata {
        PlatformMetadata::Ios(metadata) => {
            if let Some(version) = metadata.minimum_os_version.as_deref() {
                println!("Requires: iOS {version} or later");
            }
        }
        PlatformMetadata::Android(metadata) => {
            if let Some(api) = metadata.min_sdk {
                println!("Requires: Android API {api} or later");
            }
        }
    }
    if let Some(expiry) = args.artifact_ttl() {
        println!("Expires in: {}", format_duration(expiry));
    }
    if let Some(maximum) = args.max_downloads {
        println!("Download limit: {maximum}");
    }
    for link in links {
        println!("\nTunnel: {}", link.provider.name());
        println!("Access: {}", access_scope(link.provider));
        println!("Install page: {}", link.install_page_url);
        println!("Install link: {}", link.install_action_url);
        if !args.no_qr {
            match qr_code(&link.install_page_url) {
                Some(code) => println!(
                    "\nScan with the phone camera ({}):\n\n{code}",
                    link.provider.name()
                ),
                None => tracing::debug!(
                    provider = link.provider.name(),
                    "install URL could not be encoded as a QR code"
                ),
            }
        }
    }
    println!("Press Ctrl-C to stop sharing and close the tunnel.");
}

fn access_scope(provider: ExposureProvider) -> &'static str {
    match provider {
        ExposureProvider::TailscaleServe => {
            "Tailnet only (phone needs Tailscale and working tailnet DNS)"
        }
        ExposureProvider::TailscaleFunnel
        | ExposureProvider::Tailscale
        | ExposureProvider::Cloudflare => "Public internet",
    }
}

/// Render the install URL as a terminal QR code, so the phone can reach it
/// without anyone retyping or messaging a long random hostname.
fn qr_code(url: &str) -> Option<String> {
    use qrcode::QrCode;
    use qrcode::render::unicode::Dense1x2;

    let code = QrCode::new(url.as_bytes()).ok()?;
    Some(
        code.render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .quiet_zone(true)
            .build(),
    )
}

fn parse_https_port(value: &str) -> Result<u16, String> {
    let port = value
        .parse::<u16>()
        .map_err(|error| format!("invalid HTTPS port: {error}"))?;
    if port == 0 {
        Err("HTTPS port must be between 1 and 65535".into())
    } else {
        Ok(port)
    }
}

fn validate_funnel_port(port: u16) -> Result<(), String> {
    if matches!(port, 443 | 8443 | 10000) {
        Ok(())
    } else {
        Err("Tailscale Funnel supports public ports 443, 8443, and 10000".into())
    }
}

/// Parse `--timeout`, which is deliberately plain seconds: it is the spelling
/// reached for by scripts and agents, where a bare number is unambiguous.
fn parse_timeout_seconds(value: &str) -> Result<u64, String> {
    let seconds = value.trim().parse::<u64>().map_err(|_| {
        format!("invalid timeout: {value} (expected a whole number of seconds, e.g. 300)")
    })?;
    if seconds == 0 {
        return Err("timeout must be at least 1 second".into());
    }
    Ok(seconds)
}

/// Accept `90s`, `30m`, `2h`, `7d`, or a bare number of seconds.
fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (number, multiplier) = match value.chars().last() {
        Some('s') => (&value[..value.len() - 1], 1),
        Some('m') => (&value[..value.len() - 1], 60),
        Some('h') => (&value[..value.len() - 1], 60 * 60),
        Some('d') => (&value[..value.len() - 1], 60 * 60 * 24),
        _ => (value, 1),
    };
    let amount = number
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("invalid duration: {value} (try 30m, 2h, 7d)"))?;
    amount
        .checked_mul(multiplier)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("duration is too large: {value}"))
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0 => "0s".into(),
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 => format!("{}h", s / 3_600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_accept_the_common_suffixes() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604_800));
        assert!(parse_duration("later").is_err());
        assert!(parse_duration("99999999999999999999d").is_err());
    }

    #[test]
    fn durations_round_trip_through_the_banner_format() {
        for text in ["45s", "30m", "2h", "7d"] {
            assert_eq!(format_duration(parse_duration(text).unwrap()), text);
        }
    }

    #[test]
    fn the_install_url_renders_as_a_qr_code() {
        let code =
            qr_code("https://random-words.trycloudflare.com/install/artifact-1").expect("QR code");
        assert!(code.lines().count() > 10);
        assert!(code.contains('█') || code.contains('▀') || code.contains('▄'));
    }

    #[test]
    fn share_defaults_to_auto_provider_discovery() {
        let cli = Cli::try_parse_from(["remote-installer", "share", "Example.ipa"]).unwrap();
        let Command::Share(args) = cli.command;
        assert!(matches!(args.provider, ShareProvider::Auto));
        let plans = provider_plans(&args);
        assert_eq!(
            plans.iter().map(|plan| plan.provider).collect::<Vec<_>>(),
            vec![
                ExposureProvider::TailscaleServe,
                ExposureProvider::TailscaleFunnel,
                ExposureProvider::Cloudflare,
            ]
        );
        assert_eq!(plans[0].https_port, 443);
        assert_eq!(plans[1].https_port, 8443);
    }

    #[test]
    fn auto_funnel_port_avoids_the_serve_port() {
        assert_eq!(auto_funnel_port(443), 8443);
        assert_eq!(auto_funnel_port(8443), 443);
        assert_eq!(auto_funnel_port(10000), 443);
    }

    #[test]
    fn tailscale_provider_values_map_to_their_exposure_modes() {
        let cases = [
            ("tailscale-serve", ExposureProvider::TailscaleServe),
            ("tailscale-funnel", ExposureProvider::TailscaleFunnel),
            ("tailscale", ExposureProvider::TailscaleFunnel),
        ];
        for (value, expected) in cases {
            let cli = Cli::try_parse_from([
                "remote-installer",
                "share",
                "Example.ipa",
                "--provider",
                value,
            ])
            .unwrap();
            let Command::Share(args) = cli.command;
            assert_eq!(ExposureProvider::from(args.provider), expected);
        }
    }

    #[test]
    fn access_scope_explains_private_and_public_providers() {
        assert_eq!(
            access_scope(ExposureProvider::TailscaleServe),
            "Tailnet only (phone needs Tailscale and working tailnet DNS)"
        );
        assert_eq!(
            access_scope(ExposureProvider::TailscaleFunnel),
            "Public internet"
        );
        assert_eq!(
            access_scope(ExposureProvider::Cloudflare),
            "Public internet"
        );
    }

    #[test]
    fn https_port_and_funnel_port_alias_share_one_argument() {
        assert_eq!(
            share_args(&["--provider", "tailscale-serve", "--https-port", "8080"]).https_port,
            8080
        );
        assert_eq!(
            share_args(&["--provider", "tailscale-funnel", "--funnel-port", "8443"]).https_port,
            8443
        );
    }

    #[test]
    fn android_sdk_tool_overrides_are_parsed_as_one_toolchain() {
        let cli = Cli::try_parse_from([
            "remote-installer",
            "share",
            "Example.apk",
            "--apkanalyzer-bin",
            "/sdk/cmdline-tools/latest/bin/apkanalyzer",
            "--apksigner-bin",
            "/sdk/build-tools/36.0.0/apksigner",
        ])
        .unwrap();
        let Command::Share(args) = cli.command;
        assert_eq!(
            args.apkanalyzer_bin.as_deref(),
            Some(std::path::Path::new(
                "/sdk/cmdline-tools/latest/bin/apkanalyzer"
            ))
        );
        assert_eq!(
            args.apksigner_bin.as_deref(),
            Some(std::path::Path::new("/sdk/build-tools/36.0.0/apksigner"))
        );
    }

    #[test]
    fn funnel_ports_are_restricted_to_the_supported_set() {
        for port in [443, 8443, 10000] {
            assert!(validate_funnel_port(port).is_ok());
        }
        assert!(validate_funnel_port(8080).is_err());
    }

    #[test]
    fn serve_accepts_any_nonzero_u16_https_port() {
        assert_eq!(parse_https_port("1").unwrap(), 1);
        assert_eq!(parse_https_port("65535").unwrap(), 65535);
        assert!(parse_https_port("0").is_err());
    }

    fn share_args(arguments: &[&str]) -> ShareArgs {
        let mut full = vec!["remote-installer", "share", "Example.ipa"];
        full.extend_from_slice(arguments);
        let Command::Share(args) = Cli::try_parse_from(full).unwrap().command;
        args
    }

    #[test]
    fn timeout_and_expire_after_set_the_same_limit() {
        assert_eq!(
            share_args(&["--timeout", "300"]).artifact_ttl(),
            Some(Duration::from_secs(300))
        );
        assert_eq!(
            share_args(&["--expire-after", "5m"]).artifact_ttl(),
            Some(Duration::from_secs(300))
        );
        assert_eq!(share_args(&[]).artifact_ttl(), None);
    }

    /// Two spellings of one limit invite "which one wins?"; clap answers by
    /// refusing rather than silently preferring one.
    #[test]
    fn passing_both_spellings_is_refused() {
        let error = Cli::try_parse_from([
            "remote-installer",
            "share",
            "Example.ipa",
            "--timeout",
            "60",
            "--expire-after",
            "5m",
        ])
        .unwrap_err()
        .to_string();
        assert!(error.contains("cannot be used with"), "{error}");
    }

    #[test]
    fn timeout_rejects_values_that_would_expire_the_share_immediately() {
        assert_eq!(parse_timeout_seconds("300").unwrap(), 300);
        assert!(
            parse_timeout_seconds("0")
                .unwrap_err()
                .contains("at least 1 second")
        );
        // A unit belongs to --expire-after; accepting it here would silently
        // read "5m" as 5 seconds.
        assert!(parse_timeout_seconds("5m").is_err());
        assert!(parse_timeout_seconds("-5").is_err());
        assert!(parse_timeout_seconds("").is_err());
    }
}
