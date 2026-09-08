//! The install page's progress endpoint is scoped to the single grant minted
//! for that page. These checks exercise the public HTTP contract rather than
//! inspecting the service's grant table directly.

mod support;

use remote_installer::service::ShareConfig;
use reqwest::StatusCode;
use reqwest::header::{CACHE_CONTROL, CONTENT_TYPE};
use serde_json::Value;
use support::SpawnOptions;

fn attribute(html: &str, name: &str) -> String {
    let prefix = format!(r#"{name}=""#);
    html.split(&prefix)
        .nth(1)
        .and_then(|tail| tail.split_once('"').map(|(value, _)| value))
        .unwrap_or_else(|| panic!("missing {name} in install page: {html}"))
        .to_string()
}

fn install_action(html: &str) -> String {
    let marker = "id=\"install-action\"";
    let action = html
        .split(marker)
        .nth(1)
        .and_then(|tail| tail.split("href=\"").nth(1))
        .and_then(|tail| tail.split_once('"').map(|(value, _)| value))
        .unwrap_or_else(|| panic!("missing install action in install page: {html}"));
    action.replace("&amp;", "&")
}

fn manifest_url(action: &str) -> String {
    url::Url::parse(action)
        .expect("parse itms-services URL")
        .query_pairs()
        .find_map(|(name, value)| (name == "url").then(|| value.into_owned()))
        .expect("itms-services action carries manifest URL")
}

fn package_url(manifest: &str) -> String {
    manifest
        .split("<string>")
        .find(|value| value.contains("/download.ipa?download="))
        .and_then(|value| value.split("</string>").next())
        .unwrap_or_else(|| panic!("manifest has no granted package URL: {manifest}"))
        .to_string()
}

async fn status(client: &reqwest::Client, url: &str) -> Value {
    let response = client
        .get(url)
        .send()
        .await
        .expect("request progress status");
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.expect("parse progress JSON")
}

#[tokio::test]
async fn status_requires_its_grant_and_head_is_non_mutating_and_non_cacheable() {
    let server = support::spawn_server(SpawnOptions {
        share_config: ShareConfig {
            max_downloads: Some(1),
            ..ShareConfig::default()
        },
    })
    .await;
    let client = support::http_client();
    let page = client
        .get(server.url(&format!("/install/{}", server.artifact.id)))
        .send()
        .await
        .expect("request install page")
        .text()
        .await
        .expect("read install page");
    let status_url = server.url(&attribute(&page, "data-status-url"));

    let initial = status(&client, &status_url).await;
    assert_eq!(initial["phase"], "waiting");
    assert_eq!(initial["bytes_sent"], 0);
    assert_eq!(initial["total_bytes"], server.artifact_bytes.len() as u64);
    assert_eq!(initial["availability"], "installable");

    let head = client
        .head(&status_url)
        .send()
        .await
        .expect("request status HEAD");
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers().get(CACHE_CONTROL).unwrap(), "no-store");
    assert_eq!(
        head.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert!(
        head.headers()
            .get(CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    assert!(head.bytes().await.expect("read HEAD body").is_empty());
    assert_eq!(status(&client, &status_url).await, initial);

    let missing_grant = client
        .get(server.url(&format!("/api/v1/artifacts/{}/status", server.artifact.id)))
        .send()
        .await
        .expect("request status without grant");
    assert_eq!(missing_grant.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        missing_grant.headers().get(CACHE_CONTROL).unwrap(),
        "no-store"
    );
    let invalid_grant = client
        .get(server.url(&format!(
            "/api/v1/artifacts/{}/status?download=wrong",
            server.artifact.id
        )))
        .send()
        .await
        .expect("request status with invalid grant");
    assert_eq!(invalid_grant.status(), StatusCode::FORBIDDEN);
    let mismatched_status_url = status_url.replacen(&server.artifact.id, "not-this-artifact", 1);
    let wrong_artifact = client
        .get(mismatched_status_url)
        .send()
        .await
        .expect("request mismatched artifact");
    assert_eq!(wrong_artifact.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ios_manifest_reuses_the_page_grant_and_progress_deduplicates_resume_ranges() {
    let server = support::spawn_server(SpawnOptions {
        share_config: ShareConfig {
            max_downloads: Some(1),
            ..ShareConfig::default()
        },
    })
    .await;
    let client = support::http_client();
    let first_page = client
        .get(server.url(&format!("/install/{}", server.artifact.id)))
        .send()
        .await
        .expect("request first install page")
        .text()
        .await
        .expect("read first install page");
    let second_page = client
        .get(server.url(&format!("/install/{}", server.artifact.id)))
        .send()
        .await
        .expect("request second install page")
        .text()
        .await
        .expect("read second install page");
    let first_status = server.url(&attribute(&first_page, "data-status-url"));
    let second_status = server.url(&attribute(&second_page, "data-status-url"));
    assert_ne!(first_status, second_status);

    let manifest_url = manifest_url(&install_action(&first_page));
    let manifest_head = client
        .head(&manifest_url)
        .send()
        .await
        .expect("probe granted manifest");
    assert_eq!(manifest_head.status(), StatusCode::OK);
    assert_eq!(status(&client, &first_status).await["phase"], "waiting");

    let manifest = client
        .get(manifest_url)
        .send()
        .await
        .expect("request granted manifest");
    assert_eq!(manifest.status(), StatusCode::OK);
    let download = package_url(&manifest.text().await.expect("read manifest"));
    assert!(download.contains("?download="));
    assert_eq!(status(&client, &first_status).await["phase"], "preparing");

    for range in ["bytes=0-15", "bytes=8-23"] {
        let response = client
            .get(&download)
            .header("range", range)
            .send()
            .await
            .expect("request resumable package range");
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        response.bytes().await.expect("drain package range");
    }
    let resumed = status(&client, &first_status).await;
    assert_eq!(resumed["phase"], "transferring");
    assert_eq!(resumed["bytes_sent"], 24);
    assert_eq!(resumed["availability"], "installable");

    // A second page got a different grant and cannot observe this attempt.
    let untouched = status(&client, &second_status).await;
    assert_eq!(untouched["phase"], "waiting");
    assert_eq!(untouched["bytes_sent"], 0);

    let completed = client
        .get(download)
        .send()
        .await
        .expect("complete granted package download");
    assert_eq!(completed.status(), StatusCode::OK);
    completed.bytes().await.expect("drain package download");
    let final_status = status(&client, &first_status).await;
    assert_eq!(final_status["phase"], "transferred");
    assert_eq!(
        final_status["bytes_sent"],
        server.artifact_bytes.len() as u64
    );
    assert_eq!(
        final_status["total_bytes"],
        server.artifact_bytes.len() as u64
    );
    assert_eq!(final_status["availability"], "limit_reached");

    // The known grant remains readable after quota exhaustion, even though a
    // new package request would be rejected.
    assert_eq!(status(&client, &first_status).await, final_status);
}
