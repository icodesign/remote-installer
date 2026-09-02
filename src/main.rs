use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use remote_installer::artifact_input::{self, SigningPolicy};
use remote_installer::exposure::{ExposureProvider, ExposureSession};
use remote_installer::http::{self, HttpState};
use remote_installer::model::{Artifact, Availability};
use remote_installer::service::{ShareConfig, ShareService};
use tracing_subscriber::EnvFilter;
use url::Url;

const TUNNEL_STARTUP_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Parser)]
#[command(
    name = "remote-installer",
    version,
    about = "Standalone iOS app distribution over a temporary HTTPS tunnel"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Share one IPA or signed iOS .app through a temporary HTTPS tunnel.
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

  # Keep the build off Cloudflare's servers (needs a Tailscale account)
  remote-installer share MyApp.ipa --provider tailscale
";

#[derive(Debug, Args)]
struct ShareArgs {
    /// Development/ad-hoc signed IPA or iphoneos .app to share.
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
    /// Temporary tunnel provider.
    #[arg(long, value_enum, default_value_t = ShareProvider::Cloudflare)]
    provider: ShareProvider,
    /// Do not print the install link as a terminal QR code.
    #[arg(long)]
    no_qr: bool,
    /// Share an IPA even if it is unsigned, has no provisioning profile, or
    /// carries an expired or mismatched one. iOS will most likely refuse to
    /// install it.
    #[arg(long)]
    allow_unsigned: bool,
    /// Public HTTPS port used by Tailscale Funnel (Tailscale only).
    #[arg(long, value_name = "PORT", default_value_t = 443, value_parser = parse_funnel_port)]
    funnel_port: u16,
    /// Explicit path to the Tailscale CLI.
    #[arg(long, value_name = "PATH")]
    tailscale_bin: Option<PathBuf>,
    /// Explicit path to the cloudflared CLI.
    #[arg(long, value_name = "PATH")]
    cloudflared_bin: Option<PathBuf>,
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
    Tailscale,
    Cloudflare,
}

impl From<ShareProvider> for ExposureProvider {
    fn from(value: ShareProvider) -> Self {
        match value {
            ShareProvider::Tailscale => Self::Tailscale,
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
        ShareProvider::Tailscale if args.cloudflared_bin.is_some() => {
            return Err("--cloudflared-bin requires --provider cloudflare".into());
        }
        ShareProvider::Cloudflare if args.tailscale_bin.is_some() => {
            return Err("--tailscale-bin requires --provider tailscale".into());
        }
        _ => {}
    }

    let provider: ExposureProvider = args.provider.into();
    eprintln!("Validating build: {}...", args.artifact.display());
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
    let prepared = tokio::task::spawn_blocking(move || {
        artifact_input::prepare(&source, None, &input_staging, signing_policy)
    })
    .await??;
    let listener = tokio::net::TcpListener::bind(args.listen).await?;
    let local_address = listener.local_addr()?;
    let origin_url = Url::parse(&format!("http://{local_address}"))?;
    eprintln!(
        "Build validated. Starting {} (this may take a few seconds)...",
        provider.name()
    );
    let exposure_start = ExposureSession::start(
        provider,
        &origin_url,
        args.tailscale_bin.as_deref(),
        args.cloudflared_bin.as_deref(),
        args.funnel_port,
    );
    tokio::pin!(exposure_start);
    let startup_started_at = std::time::Instant::now();
    let mut startup_progress = tokio::time::interval_at(
        tokio::time::Instant::now() + TUNNEL_STARTUP_PROGRESS_INTERVAL,
        TUNNEL_STARTUP_PROGRESS_INTERVAL,
    );
    let mut exposure = loop {
        tokio::select! {
            result = &mut exposure_start => break result?,
            _ = startup_progress.tick() => eprintln!(
                "Still waiting for {}... ({} elapsed)",
                provider.name(),
                format_duration(startup_started_at.elapsed()),
            ),
        }
    };
    eprintln!("Tunnel ready. Preparing install page...");
    let setup_result = ShareService::create(
        temporary.path().join("artifacts"),
        exposure.public_base_url().clone(),
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
            if let Err(cleanup_error) = exposure.stop().await {
                tracing::error!(%cleanup_error, "failed to close tunnel after setup error");
            }
            return Err(error.into());
        }
    };
    let artifact = service.artifact().clone();
    let install_page_url = service.install_page_url(&artifact);
    let itms_services_url = service.itms_services_url(&artifact);
    print_share_banner(
        &artifact,
        &exposure,
        &install_page_url,
        &itms_services_url,
        &args,
    );

    let state = HttpState {
        service: Arc::clone(&service),
    };
    let origin_result = tokio::select! {
        result = http::run_listener(listener, state, wait_for_shutdown(Arc::clone(&service))) => result,
        result = exposure.wait_for_exit() => {
            result.map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { Box::new(error) })
        }
    };
    let cleanup_result = exposure.stop().await;
    if let Err(error) = cleanup_result {
        tracing::error!(%error, "failed to close the tunnel");
        if origin_result.is_ok() {
            return Err(error.into());
        }
    }
    origin_result
}

/// Resolve when the share should stop serving: Ctrl-C, the configured expiry,
/// or the last allowed download being claimed.
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

fn print_share_banner(
    artifact: &Artifact,
    exposure: &ExposureSession,
    install_page_url: &str,
    itms_services_url: &str,
    args: &ShareArgs,
) {
    println!("App: {}", artifact.title());
    if let Some(version) = artifact.minimum_os_version.as_deref() {
        println!("Requires: iOS {version} or later");
    }
    println!("Tunnel: {}", exposure.provider().name());
    println!("Install page: {install_page_url}");
    println!("Install link: {itms_services_url}");
    if let Some(expiry) = args.artifact_ttl() {
        println!("Expires in: {}", format_duration(expiry));
    }
    if let Some(maximum) = args.max_downloads {
        println!("Download limit: {maximum}");
    }
    if !args.no_qr {
        match qr_code(install_page_url) {
            Some(code) => println!("\nScan with the iPhone camera:\n\n{code}"),
            None => tracing::debug!("install URL could not be encoded as a QR code"),
        }
    }
    println!("Press Ctrl-C to stop sharing and close the tunnel.");
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

fn parse_funnel_port(value: &str) -> Result<u16, String> {
    let port = value
        .parse::<u16>()
        .map_err(|error| format!("invalid HTTPS port: {error}"))?;
    if matches!(port, 443 | 8443 | 10000) {
        Ok(port)
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
    fn share_defaults_to_cloudflare_quick_tunnel() {
        let cli = Cli::try_parse_from(["remote-installer", "share", "Example.ipa"]).unwrap();
        let Command::Share(args) = cli.command;
        assert!(matches!(args.provider, ShareProvider::Cloudflare));
    }

    #[test]
    fn funnel_ports_are_restricted_to_the_supported_set() {
        assert_eq!(parse_funnel_port("443").unwrap(), 443);
        assert_eq!(parse_funnel_port("8443").unwrap(), 8443);
        assert_eq!(parse_funnel_port("10000").unwrap(), 10000);
        assert!(parse_funnel_port("8080").is_err());
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
