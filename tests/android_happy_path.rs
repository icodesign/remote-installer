//! Android's browser-driven install flow: install page, direct APK download,
//! MIME metadata, and one grant spanning Range retries.

#![cfg(unix)]

mod support;

use remote_installer::model::PlatformMetadata;
use remote_installer::service::ShareConfig;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_TYPE, RANGE};
use support::SpawnOptions;

fn apk_href(html: &str) -> String {
    html.split("href=\"")
        .skip(1)
        .filter_map(|tail| tail.split_once('"').map(|(href, _)| href))
        .find(|href| href.contains("/download.apk?download="))
        .expect("Android install page should contain a granted APK URL")
        .to_string()
}

#[tokio::test]
async fn android_page_links_directly_to_the_described_apk() {
    let server = support::spawn_android_server(SpawnOptions::default()).await;
    let client = support::http_client();
    let page = client
        .get(server.url(&format!("/install/{}", server.artifact.id)))
        .send()
        .await
        .expect("request Android install page");
    assert_eq!(page.status(), StatusCode::OK);
    let html = page.text().await.expect("read install page");

    assert!(html.contains("Install Remote Installer Fixture · APK"));
    assert!(html.contains("Package name"));
    assert!(html.contains("com.rootstudio.remoteinstaller.fixture"));
    assert!(html.contains("Requires Android API 26 or later"));
    assert!(html.contains("allow installs from this browser"));
    assert!(!html.contains("itms-services://"));

    let PlatformMetadata::Android(metadata) = &server.artifact.platform_metadata else {
        panic!("fixture should produce an Android artifact");
    };
    assert_eq!(metadata.version_code.as_deref(), Some("7"));
    assert_eq!(metadata.version_name.as_deref(), Some("1.2.3"));
    assert_eq!(metadata.target_sdk, Some(36));
    assert_eq!(
        metadata.certificate_sha256.as_deref(),
        Some("95f3fc3ee59a9d33792c2fb0b8bebd63836b312e30f03d8db5855bd98731a5b7")
    );

    let manifest = client
        .get(server.url(&format!(
            "/api/v1/artifacts/{}/manifest.plist",
            server.artifact.id
        )))
        .send()
        .await
        .expect("request inapplicable iOS manifest");
    assert_eq!(manifest.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn apk_download_has_android_mime_and_matches_the_signed_fixture() {
    let server = support::spawn_android_server(SpawnOptions::default()).await;
    let client = support::http_client();
    let html = client
        .get(server.url(&format!("/install/{}", server.artifact.id)))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let response = client.get(apk_href(&html)).send().await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CONTENT_TYPE).unwrap(),
        "application/vnd.android.package-archive"
    );
    assert_eq!(response.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
    assert!(
        response
            .headers()
            .get(CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("signed-fixture.apk")
    );
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        server.artifact_bytes.as_slice()
    );
}

#[tokio::test]
async fn one_android_grant_covers_range_retries_and_closes_new_downloads_after_completion() {
    let server = support::spawn_android_server(SpawnOptions {
        share_config: ShareConfig {
            max_downloads: Some(1),
            ..ShareConfig::default()
        },
    })
    .await;
    let client = support::http_client();
    let install_page = server.url(&format!("/install/{}", server.artifact.id));
    let html = client
        .get(&install_page)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let download = apk_href(&html);

    let head = client.head(&download).send().await.unwrap();
    assert_eq!(head.status(), StatusCode::OK);

    for range in ["bytes=0-15", "bytes=16-31"] {
        let response = client
            .get(&download)
            .header(RANGE, range)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.bytes().await.unwrap().len(), 16);
    }

    // The Range responses above are successful pieces, not a completed APK
    // download. The same grant remains usable for the final full transfer,
    // which is the point at which max-downloads is spent.
    let complete = client.get(&download).send().await.unwrap();
    assert_eq!(complete.status(), StatusCode::OK);
    assert_eq!(
        complete.bytes().await.unwrap().as_ref(),
        server.artifact_bytes.as_slice()
    );

    let bare_download = server.url(&format!(
        "/api/v1/artifacts/{}/download.apk",
        server.artifact.id
    ));
    assert_eq!(
        client.get(bare_download).send().await.unwrap().status(),
        StatusCode::GONE
    );
    let spent_page = client
        .get(install_page)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(spent_page.contains("Download limit reached"));
    assert!(!spent_page.contains("/download.apk?download="));

    let wrong_extension = server.url(&format!(
        "/api/v1/artifacts/{}/download.ipa",
        server.artifact.id
    ));
    assert_eq!(
        client.get(wrong_extension).send().await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}
