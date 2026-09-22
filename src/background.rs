use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::time::{Instant, sleep};
use uuid::Uuid;

use super::ShareArgs;

type DynError = Box<dyn std::error::Error + Send + Sync>;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(300);
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const LAUNCHD_EXIT_TIMEOUT_SECONDS: u64 = 135;
const STATE_FILE: &str = "session.json";
const STDOUT_FILE: &str = "stdout.log";
const STDERR_FILE: &str = "stderr.log";
const JOB_FILE: &str = "job.plist";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Starting,
    Ready,
    Stopping,
    Stopped,
    Failed,
}

impl Phase {
    fn name(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ReadyLink {
    pub(super) tunnel: String,
    pub(super) access: String,
    pub(super) install_page: String,
    pub(super) install_link: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionState {
    schema_version: u8,
    id: String,
    label: String,
    phase: Phase,
    artifact: String,
    created_at_unix: u64,
    expires_at_unix: Option<u64>,
    worker_pid: Option<u32>,
    local_address: Option<String>,
    app: Option<String>,
    links: Vec<ReadyLink>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct SessionReport<'a> {
    session: &'a SessionState,
    running: bool,
}

#[derive(Debug, Serialize)]
struct LaunchJob {
    #[serde(rename = "Label")]
    label: String,
    #[serde(rename = "Program")]
    program: String,
    #[serde(rename = "ProgramArguments")]
    program_arguments: Vec<String>,
    #[serde(rename = "WorkingDirectory")]
    working_directory: String,
    #[serde(rename = "EnvironmentVariables")]
    environment_variables: BTreeMap<String, String>,
    #[serde(rename = "StandardOutPath")]
    standard_out_path: String,
    #[serde(rename = "StandardErrorPath")]
    standard_error_path: String,
    #[serde(rename = "RunAtLoad")]
    run_at_load: bool,
    #[serde(rename = "KeepAlive")]
    keep_alive: bool,
    #[serde(rename = "AbandonProcessGroup")]
    abandon_process_group: bool,
    #[serde(rename = "ExitTimeOut")]
    exit_time_out: u64,
}

pub(super) async fn start(mut args: ShareArgs) -> Result<(), DynError> {
    if args.artifact_ttl().is_none() {
        return Err("--background requires --expire-after or --timeout so the share cannot remain open indefinitely".into());
    }
    if args.managed_session.is_some() {
        return Err("a managed background worker cannot start another background worker".into());
    }

    make_paths_absolute(&mut args)?;
    args.background = false;
    args.no_qr = true;
    let id = Uuid::new_v4().to_string();
    let label = format!(
        "io.icodesign.remote-installer.share.{}",
        id.replace('-', "")
    );
    let directory = sessions_root()?.join(&id);
    fs::create_dir_all(&directory)?;
    args.managed_session = Some(directory.clone());

    let mut state = SessionState {
        schema_version: 1,
        id,
        label,
        phase: Phase::Starting,
        artifact: args.artifact.display().to_string(),
        created_at_unix: unix_time(),
        expires_at_unix: None,
        worker_pid: None,
        local_address: None,
        app: None,
        links: Vec::new(),
        error: None,
    };
    write_state(&directory, &state)?;

    let executable = std::env::current_exe()?.canonicalize()?;
    let working_directory = std::env::current_dir()?;
    let job = launch_job(&state, &args, &executable, &working_directory, &directory);
    plist::to_file_xml(directory.join(JOB_FILE), &job)?;

    let domain = launch_domain()?;
    let output = ProcessCommand::new("/bin/launchctl")
        .arg("bootstrap")
        .arg(&domain)
        .arg(directory.join(JOB_FILE))
        .output()?;
    if !output.status.success() {
        state.phase = Phase::Failed;
        state.error = Some(command_failure("launchctl bootstrap", &output));
        write_state(&directory, &state)?;
        return Err(state.error.clone().unwrap().into());
    }

    let ready = wait_until_ready(&directory, &domain, &state.label).await;
    match ready {
        Ok(state) => {
            print_state(&state, true, args.json)?;
            Ok(())
        }
        Err(error) => {
            let _ = bootout(&domain, &state.label);
            Err(error)
        }
    }
}

pub(super) fn status(id: &str, json: bool) -> Result<(), DynError> {
    let directory = session_directory(id)?;
    let (state, running) = reconciled_state(&directory)?;
    print_state(&state, running, json)
}

pub(super) fn logs(id: &str) -> Result<(), DynError> {
    let directory = session_directory(id)?;
    let stdout = fs::read_to_string(directory.join(STDOUT_FILE)).unwrap_or_default();
    let stderr = fs::read_to_string(directory.join(STDERR_FILE)).unwrap_or_default();
    if !stdout.is_empty() {
        println!("{stdout}");
    }
    if !stderr.is_empty() {
        eprintln!("{stderr}");
    }
    Ok(())
}

pub(super) async fn stop(id: &str, json: bool) -> Result<(), DynError> {
    let directory = session_directory(id)?;
    let (mut state, running) = reconciled_state(&directory)?;
    let domain = launch_domain()?;
    if running {
        state.phase = Phase::Stopping;
        write_state(&directory, &state)?;
        if job_is_loaded(&domain, &state.label) {
            bootout(&domain, &state.label)?;
        }
    } else if !matches!(state.phase, Phase::Stopped | Phase::Failed) {
        state.phase = Phase::Stopped;
        write_state(&directory, &state)?;
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        state = read_state(&directory)?;
        if matches!(state.phase, Phase::Stopped | Phase::Failed) {
            break;
        }
        sleep(STARTUP_POLL_INTERVAL).await;
    }
    let running = worker_is_running(&domain, &state, &directory);
    if !running && !matches!(state.phase, Phase::Stopped | Phase::Failed) {
        state.phase = Phase::Stopped;
        write_state(&directory, &state)?;
    }
    print_state(&state, running, json)
}

pub(super) fn record_worker_started(directory: &Path) -> Result<(), DynError> {
    let mut state = read_state(directory)?;
    state.worker_pid = Some(std::process::id());
    write_state(directory, &state)
}

pub(super) fn record_ready(
    directory: &Path,
    app: &str,
    local_address: SocketAddr,
    ttl: Option<Duration>,
    links: Vec<ReadyLink>,
) -> Result<(), DynError> {
    let mut state = read_state(directory)?;
    state.phase = Phase::Ready;
    state.app = Some(app.to_owned());
    state.local_address = Some(local_address.to_string());
    state.expires_at_unix = ttl.map(|duration| unix_time().saturating_add(duration.as_secs()));
    state.links = links;
    state.error = None;
    write_state(directory, &state)
}

pub(super) fn record_worker_finished(
    directory: &Path,
    error: Option<&(dyn std::error::Error + Send + Sync)>,
) -> Result<(), DynError> {
    let mut state = read_state(directory)?;
    if let Some(error) = error {
        state.phase = Phase::Failed;
        state.error = Some(error.to_string());
    } else {
        state.phase = Phase::Stopped;
    }
    write_state(directory, &state)
}

fn launch_job(
    state: &SessionState,
    args: &ShareArgs,
    executable: &Path,
    working_directory: &Path,
    directory: &Path,
) -> LaunchJob {
    let executable = executable.display().to_string();
    let mut environment_variables = BTreeMap::new();
    environment_variables.insert(
        "PATH".to_owned(),
        std::env::var("PATH").unwrap_or_else(|_| {
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_owned()
        }),
    );
    for name in [
        "HOME",
        "TMPDIR",
        "ANDROID_HOME",
        "ANDROID_SDK_ROOT",
        "RUST_LOG",
    ] {
        if let Ok(value) = std::env::var(name) {
            environment_variables.insert(name.to_owned(), value);
        }
    }
    LaunchJob {
        label: state.label.clone(),
        program: executable.clone(),
        program_arguments: worker_arguments(args, executable),
        working_directory: working_directory.display().to_string(),
        environment_variables,
        standard_out_path: directory.join(STDOUT_FILE).display().to_string(),
        standard_error_path: directory.join(STDERR_FILE).display().to_string(),
        run_at_load: true,
        keep_alive: false,
        abandon_process_group: false,
        exit_time_out: LAUNCHD_EXIT_TIMEOUT_SECONDS,
    }
}

fn worker_arguments(args: &ShareArgs, executable: String) -> Vec<String> {
    let mut result = vec![
        executable,
        "share".to_owned(),
        args.artifact.display().to_string(),
        "--timeout".to_owned(),
        args.artifact_ttl()
            .expect("background TTL validated")
            .as_secs()
            .to_string(),
        "--provider".to_owned(),
        args.provider.cli_name().to_owned(),
        "--listen".to_owned(),
        args.listen.to_string(),
        "--no-qr".to_owned(),
        "--managed-session".to_owned(),
        args.managed_session
            .as_ref()
            .expect("managed session assigned")
            .display()
            .to_string(),
    ];
    if let Some(port) = args.https_port {
        result.extend(["--https-port".to_owned(), port.to_string()]);
    }
    if let Some(maximum) = args.max_downloads {
        result.extend(["--max-downloads".to_owned(), maximum.to_string()]);
    }
    if args.allow_unsigned {
        result.push("--allow-unsigned".to_owned());
    }
    push_path_option(
        &mut result,
        "--tailscale-bin",
        args.tailscale_bin.as_deref(),
    );
    push_path_option(
        &mut result,
        "--cloudflared-bin",
        args.cloudflared_bin.as_deref(),
    );
    push_path_option(
        &mut result,
        "--apkanalyzer-bin",
        args.apkanalyzer_bin.as_deref(),
    );
    push_path_option(
        &mut result,
        "--apksigner-bin",
        args.apksigner_bin.as_deref(),
    );
    result
}

fn push_path_option(arguments: &mut Vec<String>, name: &str, value: Option<&Path>) {
    if let Some(value) = value {
        arguments.extend([name.to_owned(), value.display().to_string()]);
    }
}

fn make_paths_absolute(args: &mut ShareArgs) -> Result<(), DynError> {
    args.artifact = args.artifact.canonicalize().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not resolve artifact {}: {error}",
                args.artifact.display()
            ),
        )
    })?;
    for value in [
        &mut args.tailscale_bin,
        &mut args.cloudflared_bin,
        &mut args.apkanalyzer_bin,
        &mut args.apksigner_bin,
    ]
    .into_iter()
    .flatten()
    {
        *value = value.canonicalize()?;
    }
    Ok(())
}

async fn wait_until_ready(
    directory: &Path,
    domain: &str,
    label: &str,
) -> Result<SessionState, DynError> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut stderr_offset = 0;
    loop {
        forward_new_stderr(directory, &mut stderr_offset)?;
        let state = read_state(directory)?;
        match state.phase {
            Phase::Ready => {
                if let Some(address) = state
                    .local_address
                    .as_deref()
                    .and_then(|value| value.parse::<SocketAddr>().ok())
                    && TcpStream::connect(address).await.is_ok()
                {
                    return Ok(state);
                }
            }
            Phase::Failed => {
                return Err(state
                    .error
                    .unwrap_or_else(|| "background share failed during startup".to_owned())
                    .into());
            }
            Phase::Stopped => return Err("background share stopped before becoming ready".into()),
            Phase::Starting | Phase::Stopping => {}
        }
        if !job_is_running(domain, label) && state.worker_pid.is_some() {
            let diagnostics = fs::read_to_string(directory.join(STDERR_FILE)).unwrap_or_default();
            return Err(format!(
                "background share exited before becoming ready{}",
                if diagnostics.trim().is_empty() {
                    String::new()
                } else {
                    format!(":\n{}", diagnostics.trim())
                }
            )
            .into());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "background share did not become ready within {} seconds",
                STARTUP_TIMEOUT.as_secs()
            )
            .into());
        }
        sleep(STARTUP_POLL_INTERVAL).await;
    }
}

fn forward_new_stderr(directory: &Path, offset: &mut usize) -> Result<(), DynError> {
    let bytes = match fs::read(directory.join(STDERR_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if bytes.len() < *offset {
        *offset = 0;
    }
    if bytes.len() > *offset {
        io::stderr().write_all(&bytes[*offset..])?;
        io::stderr().flush()?;
        *offset = bytes.len();
    }
    Ok(())
}

fn print_state(state: &SessionState, running: bool, json: bool) -> Result<(), DynError> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&SessionReport {
                session: state,
                running,
            })?
        );
        return Ok(());
    }
    println!("Share: {}", state.id);
    println!(
        "Status: {}{}",
        state.phase.name(),
        if running { " (running)" } else { "" }
    );
    if let Some(app) = &state.app {
        println!("App: {app}");
    }
    for link in &state.links {
        println!("\nTunnel: {}", link.tunnel);
        println!("Access: {}", link.access);
        println!("Install page: {}", link.install_page);
    }
    if let Some(error) = &state.error {
        println!("Error: {error}");
    }
    Ok(())
}

fn sessions_root() -> Result<PathBuf, DynError> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Caches")
        .join("remote-installer")
        .join("shares"))
}

fn session_directory(id: &str) -> Result<PathBuf, DynError> {
    let id = Uuid::parse_str(id).map_err(|_| "invalid background share ID")?;
    Ok(sessions_root()?.join(id.to_string()))
}

fn state_path(directory: &Path) -> PathBuf {
    directory.join(STATE_FILE)
}

fn read_state(directory: &Path) -> Result<SessionState, DynError> {
    Ok(serde_json::from_slice(&fs::read(state_path(directory))?)?)
}

fn write_state(directory: &Path, state: &SessionState) -> Result<(), DynError> {
    fs::create_dir_all(directory)?;
    let temporary = directory.join("session.json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(state)?)?;
    fs::rename(temporary, state_path(directory))?;
    Ok(())
}

fn launch_domain() -> Result<String, DynError> {
    let output = ProcessCommand::new("/usr/bin/id").arg("-u").output()?;
    if !output.status.success() {
        return Err(command_failure("id -u", &output).into());
    }
    let uid = String::from_utf8(output.stdout)?.trim().to_owned();
    Ok(format!("gui/{uid}"))
}

fn job_is_running(domain: &str, label: &str) -> bool {
    launchctl_job(domain, label)
        .is_some_and(|output| String::from_utf8_lossy(&output.stdout).contains("state = running"))
}

fn job_is_loaded(domain: &str, label: &str) -> bool {
    launchctl_job(domain, label).is_some()
}

fn launchctl_job(domain: &str, label: &str) -> Option<std::process::Output> {
    ProcessCommand::new("/bin/launchctl")
        .arg("print")
        .arg(format!("{domain}/{label}"))
        .output()
        .ok()
        .filter(|output| output.status.success())
}

fn worker_is_running(domain: &str, state: &SessionState, directory: &Path) -> bool {
    job_is_running(domain, &state.label)
        || state
            .worker_pid
            .is_some_and(|pid| process_matches_session(pid, directory))
}

fn process_matches_session(pid: u32, directory: &Path) -> bool {
    ProcessCommand::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .is_some_and(|output| {
            let command = String::from_utf8_lossy(&output.stdout);
            command.contains("--managed-session")
                && command.contains(directory.to_string_lossy().as_ref())
        })
}

fn reconciled_state(directory: &Path) -> Result<(SessionState, bool), DynError> {
    let mut state = read_state(directory)?;
    let running = worker_is_running(&launch_domain()?, &state, directory);
    if !running {
        match state.phase {
            Phase::Starting | Phase::Ready => {
                state.phase = Phase::Failed;
                state.error = Some("background worker is no longer running".to_owned());
                write_state(directory, &state)?;
            }
            Phase::Stopping => {
                state.phase = Phase::Stopped;
                write_state(directory, &state)?;
            }
            Phase::Stopped | Phase::Failed => {}
        }
    }
    Ok((state, running))
}

fn bootout(domain: &str, label: &str) -> Result<(), DynError> {
    let output = ProcessCommand::new("/bin/launchctl")
        .arg("bootout")
        .arg(format!("{domain}/{label}"))
        .output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_failure("launchctl bootout", &output).into())
    }
}

fn command_failure(command: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("{command} failed ({}): {}", output.status, stderr.trim())
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    use crate::{Cli, Command};

    fn background_args(directory: &Path) -> ShareArgs {
        let Command::Share(mut args) = Cli::try_parse_from([
            "remote-installer",
            "share",
            "/tmp/Example.ipa",
            "--background",
            "--expire-after",
            "30m",
            "--provider",
            "tailscale-serve",
            "--max-downloads",
            "1",
        ])
        .unwrap()
        .command
        else {
            panic!("share command")
        };
        args.background = false;
        args.no_qr = true;
        args.managed_session = Some(directory.to_owned());
        args
    }

    fn state() -> SessionState {
        SessionState {
            schema_version: 1,
            id: Uuid::nil().to_string(),
            label: "io.icodesign.remote-installer.share.test".to_owned(),
            phase: Phase::Starting,
            artifact: "/tmp/Example.ipa".to_owned(),
            created_at_unix: 1,
            expires_at_unix: None,
            worker_pid: None,
            local_address: None,
            app: None,
            links: vec![],
            error: None,
        }
    }

    #[test]
    fn launchd_owns_the_native_worker_and_its_process_group() {
        let temporary = tempfile::tempdir().unwrap();
        let args = background_args(temporary.path());
        let native = Path::new("/Applications/Remote Installer/remote-installer");
        let job = launch_job(
            &state(),
            &args,
            native,
            Path::new("/tmp/project"),
            temporary.path(),
        );

        assert_eq!(job.program, native.display().to_string());
        assert_eq!(job.program_arguments[0], native.display().to_string());
        assert_eq!(job.program_arguments[1], "share");
        assert!(
            !job.program_arguments
                .iter()
                .any(|value| value == "--background")
        );
        assert!(
            job.program_arguments
                .iter()
                .any(|value| value == "--managed-session")
        );
        assert!(job.run_at_load);
        assert!(
            !job.keep_alive,
            "a failed share must not restart with a new URL"
        );
        assert!(
            !job.abandon_process_group,
            "launchd must reap tunnel descendants"
        );
        assert!(
            job.exit_time_out > 120,
            "allow the HTTP graceful drain to finish"
        );

        let plist_path = temporary.path().join("job.plist");
        plist::to_file_xml(&plist_path, &job).unwrap();
        let decoded: plist::Value = plist::from_file(plist_path).unwrap();
        let dictionary = decoded.as_dictionary().unwrap();
        assert_eq!(
            dictionary.get("Program").and_then(plist::Value::as_string),
            Some(native.to_str().unwrap())
        );
        assert_eq!(
            dictionary
                .get("AbandonProcessGroup")
                .and_then(plist::Value::as_boolean),
            Some(false)
        );
    }

    #[test]
    fn worker_arguments_preserve_the_share_limits_and_provider() {
        let temporary = tempfile::tempdir().unwrap();
        let args = background_args(temporary.path());
        let arguments = worker_arguments(&args, "/native/remote-installer".to_owned());
        let joined = arguments.join(" ");
        assert!(joined.contains("--timeout 1800"), "{joined}");
        assert!(joined.contains("--max-downloads 1"), "{joined}");
        assert!(joined.contains("--provider tailscale-serve"), "{joined}");
        assert!(joined.contains("--no-qr"), "{joined}");
        assert!(!joined.contains("--https-port"), "{joined}");

        let mut explicit = background_args(temporary.path());
        explicit.https_port = Some(10001);
        let joined = worker_arguments(&explicit, "/native/remote-installer".to_owned()).join(" ");
        assert!(joined.contains("--https-port 10001"), "{joined}");
    }

    #[test]
    fn session_ids_cannot_escape_the_managed_root() {
        assert!(session_directory("../../Library/LaunchAgents").is_err());
    }

    #[tokio::test]
    async fn background_shares_require_a_finite_expiry() {
        let Command::Share(args) = Cli::try_parse_from([
            "remote-installer",
            "share",
            "/tmp/Example.ipa",
            "--background",
        ])
        .unwrap()
        .command
        else {
            panic!("share command")
        };
        let error = start(args).await.unwrap_err().to_string();
        assert!(
            error.contains("requires --expire-after or --timeout"),
            "{error}"
        );
    }
}
