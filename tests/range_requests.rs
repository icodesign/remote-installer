//! Resumable downloads: iOS (and any HTTP client resuming a broken transfer)
//! relies on `Range` support to avoid re-downloading a partially-fetched
//! IPA, and a `Range` request must not silently eat into `max_downloads`.

mod support;

use reqwest::StatusCode;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};

use remote_installer::service::ShareConfig;
use support::SpawnOptions;

#[tokio::test]
async fn prefix_and_suffix_ranges_return_partial_content() {
    let server = support::spawn_server(SpawnOptions::default()).await;
    let client = support::http_client();
    let download_url = server.url(&format!(
        "/api/v1/artifacts/{}/download.ipa",
        server.artifact.id
    ));
    let total = server.artifact_bytes.len() as u64;

    let prefix = client
        .get(&download_url)
        .header(RANGE, "bytes=0-9")
        .send()
        .await
        .expect("request prefix range");
    assert_eq!(prefix.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        prefix.headers().get(CONTENT_RANGE).unwrap(),
        format!("bytes 0-9/{total}").as_str()
    );
    assert_eq!(prefix.headers().get(CONTENT_LENGTH).unwrap(), "10");
    let prefix_body = prefix.bytes().await.expect("read prefix body");
    assert_eq!(prefix_body.as_ref(), &server.artifact_bytes[0..10]);

    let suffix = client
        .get(&download_url)
        .header(RANGE, "bytes=-8")
        .send()
        .await
        .expect("request suffix range");
    assert_eq!(suffix.status(), StatusCode::PARTIAL_CONTENT);
    let expected_start = total - 8;
    assert_eq!(
        suffix.headers().get(CONTENT_RANGE).unwrap(),
        format!("bytes {expected_start}-{}/{total}", total - 1).as_str()
    );
    assert_eq!(suffix.headers().get(CONTENT_LENGTH).unwrap(), "8");
    let suffix_body = suffix.bytes().await.expect("read suffix body");
    assert_eq!(
        suffix_body.as_ref(),
        &server.artifact_bytes[(total as usize - 8)..]
    );
}

#[tokio::test]
async fn range_past_the_end_of_the_file_is_416() {
    let server = support::spawn_server(SpawnOptions::default()).await;
    let client = support::http_client();

    let response = client
        .get(server.url(&format!(
            "/api/v1/artifacts/{}/download.ipa",
            server.artifact.id
        )))
        .header(RANGE, "bytes=999999999-")
        .send()
        .await
        .expect("request out-of-range");

    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
}

#[tokio::test]
async fn a_manifest_grant_allows_range_retries_but_counts_one_ota_attempt() {
    let server = support::spawn_server(SpawnOptions {
        share_config: ShareConfig {
            max_downloads: Some(1),
            ..ShareConfig::default()
        },
    })
    .await;
    let client = support::http_client();
    let manifest = client
        .get(server.url(&format!(
            "/api/v1/artifacts/{}/manifest.plist",
            server.artifact.id
        )))
        .send()
        .await
        .expect("request manifest")
        .text()
        .await
        .expect("read manifest");
    let download_url = manifest
        .split("<string>")
        .find(|value| value.contains("/download.ipa?download="))
        .and_then(|value| value.split("</string>").next())
        .expect("manifest download grant")
        .to_string();

    // Several range requests for the same manifest-issued grant are one OTA
    // attempt and must all remain usable for resume/retry.
    for _ in 0..3 {
        let response = client
            .get(&download_url)
            .header(RANGE, "bytes=0-3")
            .send()
            .await
            .expect("request range");
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    }

    // A full retry with the same grant is still part of that one attempt.
    let first_full = client
        .get(&download_url)
        .send()
        .await
        .expect("request full download");
    assert_eq!(first_full.status(), StatusCode::OK);

    // A new manifest cannot mint another grant once the one-download quota is
    // exhausted.
    let second_manifest = client
        .get(server.url(&format!(
            "/api/v1/artifacts/{}/manifest.plist",
            server.artifact.id
        )))
        .send()
        .await
        .expect("request second manifest");
    assert_eq!(second_manifest.status(), StatusCode::GONE);
}

#[tokio::test]
async fn a_direct_range_without_a_grant_consumes_one_download_slot() {
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

    let first_range = client
        .get(&download_url)
        .header(RANGE, "bytes=0-3")
        .send()
        .await
        .expect("request direct range");
    assert_eq!(first_range.status(), StatusCode::PARTIAL_CONTENT);

    let second_range = client
        .get(&download_url)
        .header(RANGE, "bytes=4-7")
        .send()
        .await
        .expect("request second direct range");
    assert_eq!(second_range.status(), StatusCode::GONE);
}
