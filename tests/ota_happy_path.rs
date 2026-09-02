//! The OTA install flow an iPhone actually walks: install page, manifest, icon,
//! and the IPA download itself. These are the routes that must never break,
//! because a silent regression here is a silent data-loss bug for anyone trying
//! to install a build.

mod support;

use reqwest::StatusCode;
use reqwest::header::{ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use sha2::{Digest, Sha256};

use support::SpawnOptions;

#[tokio::test]
async fn install_page_serves_secure_html_with_the_itms_link() {
    let server = support::spawn_server(SpawnOptions::default()).await;
    let client = support::http_client();

    let response = client
        .get(server.url(&format!("/install/{}", server.artifact.id)))
        .send()
        .await
        .expect("request install page");

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(content_type.starts_with("text/html"), "{content_type}");
    assert_eq!(
        response.headers().get("content-security-policy").unwrap(),
        "default-src 'none'; style-src 'unsafe-inline'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    assert_eq!(
        response.headers().get("referrer-policy").unwrap(),
        "no-referrer"
    );
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");

    let body = response.text().await.expect("read install page body");
    assert!(
        body.contains(&server.artifact.file_name),
        "install page should show the app name: {body}"
    );
    assert!(
        body.contains("itms-services://"),
        "install page should carry an itms-services href: {body}"
    );
}

#[tokio::test]
async fn manifest_advertises_the_icon_assets() {
    let server = support::spawn_server(SpawnOptions::default()).await;
    let client = support::http_client();

    let response = client
        .get(server.url(&format!(
            "/api/v1/artifacts/{}/manifest.plist",
            server.artifact.id
        )))
        .send()
        .await
        .expect("request manifest");

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_type.starts_with("application/xml"),
        "{content_type}"
    );

    let body = response.text().await.expect("read manifest body");
    assert!(body.contains("software-package"), "{body}");
    assert!(body.contains("display-image"), "{body}");
    assert!(body.contains("full-size-image"), "{body}");
}

#[tokio::test]
async fn icon_download_serves_the_png_bytes() {
    let server = support::spawn_server(SpawnOptions::default()).await;
    let client = support::http_client();

    let response = client
        .get(server.url(&format!(
            "/api/v1/artifacts/{}/icon.png",
            server.artifact.id
        )))
        .send()
        .await
        .expect("request icon");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers().get(CONTENT_TYPE).unwrap(), "image/png");
    let content_length: u64 = response
        .headers()
        .get(CONTENT_LENGTH)
        .expect("content-length present")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();

    let body = response.bytes().await.expect("read icon body");
    assert_eq!(content_length, body.len() as u64);
    assert!(
        body.starts_with(b"\x89PNG\r\n\x1a\n"),
        "icon body should start with the PNG magic bytes"
    );
}

#[tokio::test]
async fn ipa_download_serves_full_bytes_matching_the_recorded_sha256() {
    let server = support::spawn_server(SpawnOptions::default()).await;
    let client = support::http_client();

    let response = client
        .get(server.url(&format!(
            "/api/v1/artifacts/{}/download.ipa",
            server.artifact.id
        )))
        .send()
        .await
        .expect("request download");

    assert_eq!(response.status(), StatusCode::OK);
    let content_length: u64 = response
        .headers()
        .get(CONTENT_LENGTH)
        .expect("content-length present")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(content_length, server.artifact_bytes.len() as u64);
    assert_eq!(response.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
    let content_disposition = response
        .headers()
        .get(CONTENT_DISPOSITION)
        .expect("content-disposition present")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_disposition.contains(&server.artifact.file_name),
        "{content_disposition}"
    );

    let body = response.bytes().await.expect("read download body");
    assert_eq!(body.as_ref(), server.artifact_bytes.as_slice());
    // sha2 0.11's digest output no longer implements LowerHex directly;
    // format it byte-by-byte the same way src/ipa.rs's sha256_file does.
    let hash: String = Sha256::digest(&body)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(hash, server.artifact.sha256);
}
