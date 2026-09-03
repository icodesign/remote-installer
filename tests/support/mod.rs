//! Shared end-to-end test harness.
//!
//! Every integration-test binary under `tests/` pulls this in with
//! `mod support;`. Each binary compiles its own copy of this file, so a
//! helper unused by one binary is expected — hence the blanket allow below
//! rather than trimming helpers per file.
#![allow(dead_code)]

use std::io::Cursor;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use remote_installer::artifact_input;
use remote_installer::http::{self, HttpState};
use remote_installer::model::Artifact;
use remote_installer::service::{ShareConfig, ShareService};

use reqwest::Client;
use reqwest::redirect::Policy;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use url::Url;

/// Per-share limits an individual test wants to vary. Everything else about
/// the harness (workspace, fixture artifact, listener) is fixed.
#[derive(Default)]
pub struct SpawnOptions {
    pub share_config: ShareConfig,
}

/// A running origin plus everything a test needs to talk to it. Dropping this
/// signals `run_listener` to stop accepting new connections and drain what's
/// in flight; tests don't need to await that explicitly.
pub struct SpawnedServer {
    pub base_url: String,
    /// The fixture IPA, imported through the same local path used by `share`.
    pub artifact: Artifact,
    /// The exact bytes of the fixture IPA on disk, so tests can assert
    /// downloads and ranges against ground truth instead of recomputing it.
    pub artifact_bytes: Vec<u8>,
    // Kept alive only so the directory isn't removed out from under the
    // still-running origin.
    _state_dir: tempfile::TempDir,
    shutdown: Option<oneshot::Sender<()>>,
}

impl SpawnedServer {
    /// Build a full URL for a path on this origin.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

impl Drop for SpawnedServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// Start a real origin bound to an OS-assigned loopback port, with a single
/// fixture artifact already imported. Individual tests vary limits through
/// `SpawnOptions`; everything else (temporary workspace, local repository,
/// fixture IPA) is identical across tests so failures are easy to compare.
pub async fn spawn_server(options: SpawnOptions) -> SpawnedServer {
    let state_dir = tempfile::tempdir().expect("create temp state dir");

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let local_addr: SocketAddr = listener.local_addr().expect("listener local address");
    let base_url = format!("http://127.0.0.1:{}", local_addr.port());
    let public_base_url = Url::parse(&base_url).expect("parse public base url");

    let fixture_path = state_dir.path().join("Example.ipa");
    let artifact_bytes = fixtures::write_example_ipa(&fixture_path);
    let prepared = artifact_input::prepare(
        &fixture_path,
        None,
        &state_dir.path().join("staging"),
        artifact_input::SigningPolicy::Trusted,
    )
    .expect("prepare fixture IPA");
    let service = ShareService::create(
        state_dir.path().join("workspace"),
        public_base_url,
        &prepared,
        options.share_config,
    )
    .await
    .expect("construct ShareService");
    let artifact = service.artifact().clone();

    let state = HttpState {
        service: std::sync::Arc::new(service),
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let shutdown = async move {
            let _ = shutdown_rx.await;
        };
        if let Err(error) = http::run_listener(listener, state, shutdown).await {
            eprintln!("test origin exited with an error: {error}");
        }
    });

    SpawnedServer {
        base_url,
        artifact,
        artifact_bytes,
        _state_dir: state_dir,
        shutdown: Some(shutdown_tx),
    }
}

/// Android counterpart to `spawn_server`, using the checked-in signed APK and
/// deterministic stand-ins for Android SDK metadata output. Signature-command
/// failure behavior is covered by `apk` unit tests; this harness owns the HTTP
/// composition from a prepared Android artifact onward.
#[cfg(unix)]
pub async fn spawn_android_server(options: SpawnOptions) -> SpawnedServer {
    use remote_installer::apk::ApkToolchain;
    use std::os::unix::fs::PermissionsExt;

    let state_dir = tempfile::tempdir().expect("create temp state dir");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let local_addr: SocketAddr = listener.local_addr().expect("listener local address");
    let base_url = format!("http://127.0.0.1:{}", local_addr.port());
    let public_base_url = Url::parse(&base_url).expect("parse public base url");

    let fixture_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/android/signed-fixture.apk");
    let artifact_bytes = std::fs::read(&fixture_path).expect("read signed APK fixture");
    let analyzer = state_dir.path().join("apkanalyzer");
    std::fs::write(
        &analyzer,
        r#"#!/bin/sh
case "$1 $2" in
  "manifest print") printf '%s\n' '<manifest package="com.rootstudio.remoteinstaller.fixture"><application android:label="Remote Installer Fixture" /></manifest>' ;;
  "manifest application-id") printf '%s\n' 'com.rootstudio.remoteinstaller.fixture' ;;
  "manifest version-code") printf '%s\n' '7' ;;
  "manifest version-name") printf '%s\n' '1.2.3' ;;
  "manifest min-sdk") printf '%s\n' '26' ;;
  "manifest target-sdk") printf '%s\n' '36' ;;
  *) exit 64 ;;
esac
"#,
    )
    .expect("write fake apkanalyzer");
    let signer = state_dir.path().join("apksigner");
    std::fs::write(
        &signer,
        "#!/bin/sh\nprintf '%s\\n' 'Signer #1 certificate SHA-256 digest: 95f3fc3ee59a9d33792c2fb0b8bebd63836b312e30f03d8db5855bd98731a5b7'\n",
    )
    .expect("write fake apksigner");
    for tool in [&analyzer, &signer] {
        let mut permissions = std::fs::metadata(tool).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(tool, permissions).unwrap();
    }
    let toolchain = ApkToolchain::new(analyzer, signer);
    let prepared = artifact_input::prepare_with_apk_toolchain(
        &fixture_path,
        None,
        &state_dir.path().join("staging"),
        artifact_input::SigningPolicy::Required,
        Some(&toolchain),
    )
    .expect("prepare fixture APK");
    let service = ShareService::create(
        state_dir.path().join("workspace"),
        public_base_url,
        &prepared,
        options.share_config,
    )
    .await
    .expect("construct Android ShareService");
    let artifact = service.artifact().clone();
    let state = HttpState {
        service: std::sync::Arc::new(service),
    };
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let shutdown = async move {
            let _ = shutdown_rx.await;
        };
        if let Err(error) = http::run_listener(listener, state, shutdown).await {
            eprintln!("test Android origin exited with an error: {error}");
        }
    });

    SpawnedServer {
        base_url,
        artifact,
        artifact_bytes,
        _state_dir: state_dir,
        shutdown: Some(shutdown_tx),
    }
}

/// A `reqwest::Client` with redirects disabled, so an unexpected redirect is
/// observable instead of being followed transparently.
pub fn http_client() -> Client {
    Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .expect("build reqwest client")
}

pub mod fixtures {
    use super::*;

    /// A minimal, deliberately-invalid-as-an-image PNG that is nonetheless
    /// valid enough for the icon extractor: real signature and IHDR chunk
    /// with the declared width/height, everything after that is filler.
    /// Mirrors the `png` helper in `src/service.rs`'s own test module.
    pub fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&[0; 4]);
        bytes
    }

    /// Writes a synthetic single-app IPA (Payload/Example.app/Info.plist with
    /// a bundle identifier, version, minimum OS, and a standalone PNG icon)
    /// to `writer`. Mirrors the `example_ipa` helper in `src/service.rs`'s
    /// own test module, which is the shape `ipa::inspect` expects.
    fn write_ipa<W: std::io::Write + std::io::Seek>(writer: W) {
        let mut writer = zip::ZipWriter::new(writer);
        writer
            .start_file::<_, ()>(
                "Payload/Example.app/Info.plist",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        let info = plist::Value::Dictionary(
            [
                (
                    "CFBundleIdentifier".to_string(),
                    plist::Value::String("com.example.app".into()),
                ),
                (
                    "CFBundleVersion".to_string(),
                    plist::Value::String("1".into()),
                ),
                (
                    "MinimumOSVersion".to_string(),
                    plist::Value::String("16.0".into()),
                ),
                (
                    "CFBundleIconFiles".to_string(),
                    plist::Value::Array(vec![plist::Value::String("AppIcon60x60".into())]),
                ),
            ]
            .into_iter()
            .collect(),
        );
        info.to_writer_xml(&mut writer).unwrap();
        writer
            .start_file::<_, ()>(
                "Payload/Example.app/AppIcon60x60@3x.png",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        std::io::Write::write_all(&mut writer, &png(180, 180)).unwrap();
        writer.finish().unwrap();
    }

    /// Writes the fixture IPA to `path` and returns the exact bytes written,
    /// so callers have ground truth for download/range assertions without
    /// re-reading the file (and without trusting the zip writer to be
    /// byte-for-byte deterministic across runs).
    pub fn write_example_ipa(path: &Path) -> Vec<u8> {
        let bytes = example_ipa_bytes();
        std::fs::write(path, &bytes).expect("write fixture IPA");
        bytes
    }

    fn example_ipa_bytes() -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        write_ipa(&mut buffer);
        buffer.into_inner()
    }
}
