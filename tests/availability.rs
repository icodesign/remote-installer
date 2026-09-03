//! What a spent or expired share link actually serves.
//!
//! The install page and the icon must outlive the download quota. iOS fetches
//! the home-screen icon *while* the package it just spent the slot on is still
//! downloading, and someone whose install failed needs the page to tell them
//! why rather than a bare 410 from the browser.

mod support;

use reqwest::StatusCode;
use reqwest::header::CONTENT_TYPE;

use remote_installer::service::ShareConfig;
use support::SpawnOptions;

async fn spend_the_only_download(server: &support::SpawnedServer, client: &reqwest::Client) {
    let response = client
        .get(server.url(&format!(
            "/api/v1/artifacts/{}/download.ipa",
            server.artifact.id
        )))
        .send()
        .await
        .expect("request download");
    assert_eq!(response.status(), StatusCode::OK);
    response.bytes().await.expect("drain download body");
}

#[tokio::test]
async fn a_spent_quota_leaves_the_install_page_and_icon_readable() {
    let server = support::spawn_server(SpawnOptions {
        share_config: ShareConfig {
            max_downloads: Some(1),
            ..ShareConfig::default()
        },
    })
    .await;
    let client = support::http_client();
    spend_the_only_download(&server, &client).await;

    let page = client
        .get(server.url(&format!("/install/{}", server.artifact.id)))
        .send()
        .await
        .expect("request install page");
    assert_eq!(page.status(), StatusCode::OK);
    let body = page.text().await.expect("read install page");
    assert!(body.contains("Download limit reached"), "{body}");
    assert!(!body.contains("itms-services://"), "{body}");
    // The build is still identified, so the reader knows they are in the
    // right place and which build ran out.
    let remote_installer::model::PlatformMetadata::Ios(metadata) =
        &server.artifact.platform_metadata
    else {
        panic!("fixture should be an iOS artifact");
    };
    assert!(body.contains(&metadata.bundle_identifier), "{body}");

    let icon = client
        .get(server.url(&format!(
            "/api/v1/artifacts/{}/icon.png",
            server.artifact.id
        )))
        .send()
        .await
        .expect("request icon");
    assert_eq!(icon.status(), StatusCode::OK);
    assert_eq!(icon.headers().get(CONTENT_TYPE).unwrap(), "image/png");
}

#[tokio::test]
async fn a_spent_quota_still_closes_the_install_routes() {
    let server = support::spawn_server(SpawnOptions {
        share_config: ShareConfig {
            max_downloads: Some(1),
            ..ShareConfig::default()
        },
    })
    .await;
    let client = support::http_client();
    spend_the_only_download(&server, &client).await;

    for path in [
        format!("/api/v1/artifacts/{}/manifest.plist", server.artifact.id),
        format!("/api/v1/artifacts/{}/download.ipa", server.artifact.id),
    ] {
        let response = client
            .get(server.url(&path))
            .send()
            .await
            .expect("request install route");
        assert_eq!(response.status(), StatusCode::GONE, "{path}");
    }
}

#[tokio::test]
async fn an_expired_share_explains_itself_on_the_install_page() {
    let server = support::spawn_server(SpawnOptions {
        share_config: ShareConfig {
            artifact_ttl: Some(std::time::Duration::ZERO),
            ..ShareConfig::default()
        },
    })
    .await;
    let client = support::http_client();

    let page = client
        .get(server.url(&format!("/install/{}", server.artifact.id)))
        .send()
        .await
        .expect("request install page");
    assert_eq!(page.status(), StatusCode::OK);
    let body = page.text().await.expect("read install page");
    assert!(body.contains("Link expired"), "{body}");
    assert!(!body.contains("itms-services://"), "{body}");

    let manifest = client
        .get(server.url(&format!(
            "/api/v1/artifacts/{}/manifest.plist",
            server.artifact.id
        )))
        .send()
        .await
        .expect("request manifest");
    assert_eq!(manifest.status(), StatusCode::GONE);
}

/// A HEAD is how a proxy or a curious client probes the download; it must not
/// quietly spend the recipient's only install attempt.
#[tokio::test]
async fn head_on_the_download_does_not_spend_a_download_slot() {
    let server = support::spawn_server(SpawnOptions {
        share_config: ShareConfig {
            max_downloads: Some(1),
            ..ShareConfig::default()
        },
    })
    .await;
    let client = support::http_client();
    let download_url = server.url(&format!(
        "/api/v1/artifacts/{}/download.ipa",
        server.artifact.id
    ));

    for _ in 0..3 {
        let head = client
            .head(&download_url)
            .send()
            .await
            .expect("request HEAD");
        assert_eq!(head.status(), StatusCode::OK);
        assert!(head.bytes().await.expect("read HEAD body").is_empty());
    }

    // The real download still has its slot.
    let response = client.get(&download_url).send().await.expect("request GET");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.bytes().await.expect("read body");
    assert_eq!(body.as_ref(), server.artifact_bytes.as_slice());
}

/// `--timeout` is only useful if elapsing actually tears the origin down. This
/// composes the same pieces `share` does — the service's shutdown signal wired
/// into `run_listener` — and checks the socket really stops answering.
#[tokio::test]
async fn the_origin_shuts_itself_down_when_the_share_expires() {
    use remote_installer::artifact_input::{self, SigningPolicy};
    use remote_installer::http::{self, HttpState};
    use remote_installer::service::ShareService;
    use std::sync::Arc;

    let state_dir = tempfile::tempdir().expect("temp dir");
    let fixture = state_dir.path().join("Example.ipa");
    support::fixtures::write_example_ipa(&fixture);
    let prepared = artifact_input::prepare(
        &fixture,
        None,
        &state_dir.path().join("staging"),
        SigningPolicy::Trusted,
    )
    .expect("prepare fixture");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let base_url = format!("http://127.0.0.1:{port}");

    let service = Arc::new(
        ShareService::create(
            state_dir.path().join("workspace"),
            url::Url::parse(&base_url).expect("url"),
            &prepared,
            ShareConfig {
                artifact_ttl: Some(std::time::Duration::from_secs(1)),
                ..ShareConfig::default()
            },
        )
        .await
        .expect("service"),
    );
    let artifact_id = service.artifact().id.clone();

    let shutdown_service = Arc::clone(&service);
    let origin = tokio::spawn(async move {
        http::run_listener(
            listener,
            HttpState {
                service: Arc::clone(&service),
            },
            async move {
                shutdown_service.wait_until_unavailable().await;
            },
        )
        .await
    });

    let client = support::http_client();
    let install_url = format!("{base_url}/install/{artifact_id}");
    assert_eq!(
        client
            .get(&install_url)
            .send()
            .await
            .expect("serving before the timeout")
            .status(),
        StatusCode::OK
    );

    // The origin should stop on its own, with nobody signalling it.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(15), origin)
        .await
        .expect("origin should shut itself down once the share expires")
        .expect("origin task panicked");
    assert!(outcome.is_ok(), "clean shutdown, not an error: {outcome:?}");

    assert!(
        client.get(&install_url).send().await.is_err(),
        "the port should be closed once the origin has shut down"
    );
}

/// The counterpart guarantee to prompt shutdown: closing idle connections must
/// not cut off a phone that is part-way through an install. A build big enough
/// to still be streaming when shutdown fires has to arrive whole anyway.
#[tokio::test]
async fn shutdown_lets_an_in_flight_download_finish() {
    use remote_installer::artifact_input::{self, SigningPolicy};
    use remote_installer::http::{self, HttpState};
    use remote_installer::service::ShareService;
    use std::sync::Arc;

    let state_dir = tempfile::tempdir().expect("temp dir");
    let fixture = state_dir.path().join("Big.ipa");
    let expected_filler = vec![0x5a_u8; 24 * 1024 * 1024];
    {
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&fixture).expect("create"));
        let options = zip::write::FileOptions::<()>::default()
            .compression_method(zip::CompressionMethod::Stored);
        writer
            .start_file::<_, ()>("Payload/Big.app/Info.plist", options)
            .expect("plist entry");
        plist::Value::Dictionary(
            [
                (
                    "CFBundleIdentifier".to_string(),
                    plist::Value::String("com.example.big".into()),
                ),
                (
                    "CFBundleVersion".to_string(),
                    plist::Value::String("1".into()),
                ),
            ]
            .into_iter()
            .collect(),
        )
        .to_writer_xml(&mut writer)
        .expect("write plist");
        writer
            .start_file::<_, ()>("Payload/Big.app/filler.bin", options)
            .expect("filler entry");
        std::io::Write::write_all(&mut writer, &expected_filler).expect("write filler");
        writer.finish().expect("finish zip");
    }
    let total = std::fs::metadata(&fixture).expect("metadata").len();
    assert!(
        total > 16 * 1024 * 1024,
        "fixture must exceed socket buffers"
    );

    let prepared = artifact_input::prepare(
        &fixture,
        None,
        &state_dir.path().join("staging"),
        SigningPolicy::Trusted,
    )
    .expect("prepare fixture");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let base_url = format!("http://127.0.0.1:{port}");
    let service = ShareService::create(
        state_dir.path().join("workspace"),
        url::Url::parse(&base_url).expect("url"),
        &prepared,
        ShareConfig::default(),
    )
    .await
    .expect("service");
    let artifact_id = service.artifact().id.clone();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let origin = tokio::spawn(async move {
        http::run_listener(
            listener,
            HttpState {
                service: Arc::new(service),
            },
            async move {
                let _ = shutdown_rx.await;
            },
        )
        .await
    });

    let client = support::http_client();
    // Headers have arrived, so the body is mid-flight on the wire.
    let response = client
        .get(format!(
            "{base_url}/api/v1/artifacts/{artifact_id}/download.ipa"
        ))
        .send()
        .await
        .expect("start download");
    assert_eq!(response.status(), StatusCode::OK);

    shutdown_tx.send(()).expect("signal shutdown");

    let body = response
        .bytes()
        .await
        .expect("download must survive shutdown");
    assert_eq!(
        body.len() as u64,
        total,
        "the whole build should arrive despite shutdown starting mid-transfer"
    );
    assert_eq!(
        std::fs::read(&fixture).expect("read fixture"),
        body.as_ref()
    );

    origin
        .await
        .expect("origin task panicked")
        .expect("clean shutdown");
}
