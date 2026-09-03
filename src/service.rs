use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::{Mutex, Notify};
use url::Url;

use crate::artifact_input::{ArtifactInputError, PreparedArtifact};
use crate::ipa::{self, ManifestAssets};
use crate::model::{Artifact, Availability, Platform, PlatformMetadata};

/// A manifest-issued download grant is intentionally short-lived. It covers
/// all Range requests for one OTA attempt, while preventing a copied grant
/// from becoming a permanent second public URL.
const DOWNLOAD_GRANT_TTL: Duration = Duration::from_secs(30 * 60);
/// One share session serves one artifact to a handful of devices, so a small
/// ring of live grants is ample. When it is full the oldest grant is evicted
/// rather than the request being refused: refusing to mint a grant would let
/// anyone holding the link brick the manifest route for half an hour just by
/// reloading the install page.
const MAX_DOWNLOAD_GRANTS: usize = 16;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("service I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("artifact preparation failed: {0}")]
    ArtifactInput(#[from] ArtifactInputError),
    #[error("resource not found: {0}")]
    NotFound(String),
    #[error("resource is no longer available: {0}")]
    Gone(String),
    #[error("share request is forbidden: {0}")]
    Forbidden(String),
}

/// Per-share limits. A share session is intentionally ephemeral: the
/// artifact and its bytes disappear when the command exits.
#[derive(Debug, Clone, Default)]
pub struct ShareConfig {
    /// How long the share remains installable.
    pub artifact_ttl: Option<Duration>,
    /// How many OTA download attempts the share allows. A manifest-issued
    /// grant may cover multiple Range requests for one attempt.
    pub max_downloads: Option<u64>,
}

/// Runtime state for one `remote-installer share` session.
///
/// The artifact itself is immutable, so the only mutable state is the spent
/// download count (an atomic) and the live grant table (a small map). That is
/// deliberately not hidden behind a repository abstraction: this process owns
/// exactly one installable package in one temporary directory, and public downloads always
/// pass through this service, so there is no second URL or credential-bearing
/// storage path for a client to bypass.
pub struct ShareService {
    public_base_url: Url,
    artifact: Artifact,
    package_path: PathBuf,
    icon_path: Option<PathBuf>,
    /// When the share stops serving, if `--timeout`/`--expire-after` was given.
    ///
    /// A monotonic deadline rather than a wall-clock timestamp: nothing here is
    /// persisted across runs, so wall-clock buys nothing, and a `--timeout 30`
    /// should still mean thirty seconds if the system clock is adjusted
    /// mid-share. Whole-second wall-clock arithmetic also made short timeouts
    /// wrong — created at T.9 with a 1s TTL, the share expired 0.1s later.
    expires_at: Option<Instant>,
    max_downloads: Option<u64>,
    /// Claimed OTA download attempts.
    download_count: AtomicU64,
    /// Fires once the quota is spent, so `share` can close the tunnel instead
    /// of idling on a link that will only ever answer 410.
    quota_spent: Notify,
    /// Grants are issued when a manifest is requested and claimed by the first
    /// download request. A claimed grant stays valid for Range retries
    /// belonging to that same OTA attempt.
    download_grants: Mutex<HashMap<String, DownloadGrant>>,
}

struct DownloadGrant {
    issued_at: Instant,
    expires_at: Instant,
    claimed: bool,
}

impl ShareService {
    /// Stage the prepared package (and its icon, when it has one) into
    /// `workspace_dir` and build the session that serves them.
    ///
    /// The bytes are copied rather than served from wherever the caller found
    /// them: a share can outlive the build that produced it, and serving a
    /// file someone is concurrently rebuilding would hand a device a torn package.
    pub async fn create(
        workspace_dir: impl Into<PathBuf>,
        public_base_url: Url,
        prepared: &PreparedArtifact,
        config: ShareConfig,
    ) -> Result<Self, ServiceError> {
        let workspace_dir = workspace_dir.into();
        tokio::fs::create_dir_all(&workspace_dir).await?;

        let metadata = prepared.metadata().clone();
        let id = format!("artifact-{}", uuid::Uuid::new_v4());
        let extension = match &metadata.platform_metadata {
            PlatformMetadata::Ios(_) => "ipa",
            PlatformMetadata::Android(_) => "apk",
        };
        let package_path = workspace_dir.join(format!("{id}.{extension}"));
        tokio::fs::copy(prepared.path(), &package_path).await?;

        let icon_path = match metadata.icon_png.as_ref() {
            Some(icon) => {
                let icon_path = workspace_dir.join(format!("{id}.icon.png"));
                tokio::fs::write(&icon_path, icon).await?;
                Some(icon_path)
            }
            None => {
                tracing::warn!(
                    artifact_id = %id,
                    "artifact has no standalone PNG icon; Assets.car-only icons are not extracted"
                );
                None
            }
        };

        let artifact = Artifact {
            id,
            file_name: metadata.file_name,
            byte_count: metadata.byte_count,
            sha256: metadata.sha256,
            display_name: metadata.display_name,
            platform_metadata: metadata.platform_metadata,
            has_icon: icon_path.is_some(),
        };

        Ok(Self {
            public_base_url,
            artifact,
            package_path,
            icon_path,
            expires_at: config
                .artifact_ttl
                .map(|ttl| Instant::now().checked_add(ttl).unwrap_or_else(far_future)),
            max_downloads: config.max_downloads,
            download_count: AtomicU64::new(0),
            quota_spent: Notify::new(),
            download_grants: Mutex::new(HashMap::new()),
        })
    }

    /// The artifact, for read-only presentation.
    ///
    /// The install page and the icon stay reachable after the download quota
    /// is spent. A device that failed to install — wrong provisioning
    /// profile, not enough storage — needs to see *why*, and iOS fetches the
    /// home-screen icon while the package is still downloading; gating either
    /// on the quota turns a spent slot into a blank 410.
    pub fn artifact(&self) -> &Artifact {
        &self.artifact
    }

    pub fn availability(&self) -> Availability {
        if self.expires_at.is_some_and(|at| Instant::now() >= at) {
            return Availability::Expired;
        }
        if self
            .max_downloads
            .is_some_and(|maximum| self.download_count.load(Ordering::Relaxed) >= maximum)
        {
            return Availability::LimitReached;
        }
        Availability::Installable
    }

    /// The artifact for a route that leads to an actual install. Returns
    /// `Gone` rather than `NotFound` once a link is used up, so the client can
    /// tell an exhausted link from a bad one.
    pub fn servable_artifact(&self, artifact_id: &str) -> Result<&Artifact, ServiceError> {
        self.match_id(artifact_id)?;
        match self.availability() {
            Availability::Installable => Ok(&self.artifact),
            Availability::Expired => Err(ServiceError::Gone("this share link has expired".into())),
            Availability::LimitReached => Err(ServiceError::Gone(
                "this share link has reached its download limit".into(),
            )),
        }
    }

    /// Resolve the artifact for a presentation route, which only has to exist.
    pub fn viewable_artifact(&self, artifact_id: &str) -> Result<&Artifact, ServiceError> {
        self.match_id(artifact_id).map(|()| &self.artifact)
    }

    fn match_id(&self, artifact_id: &str) -> Result<(), ServiceError> {
        if self.artifact.id == artifact_id {
            Ok(())
        } else {
            Err(ServiceError::NotFound("artifact not found".into()))
        }
    }

    /// Issue an opaque, short-lived grant for the next package download. The grant
    /// is intentionally not counted until the first download request arrives:
    /// opening a manifest must not consume a download slot.
    pub async fn issue_download_grant(&self, artifact_id: &str) -> Result<String, ServiceError> {
        self.servable_artifact(artifact_id)?;
        let mut grants = self.download_grants.lock().await;
        let now = Instant::now();
        grants.retain(|_, grant| grant.expires_at > now);
        while grants.len() >= MAX_DOWNLOAD_GRANTS {
            let Some(oldest) = grants
                .iter()
                .min_by_key(|(_, grant)| grant.issued_at)
                .map(|(token, _)| token.clone())
            else {
                break;
            };
            grants.remove(&oldest);
        }
        let token = uuid::Uuid::new_v4().to_string();
        grants.insert(
            token.clone(),
            DownloadGrant {
                issued_at: now,
                expires_at: now + DOWNLOAD_GRANT_TTL,
                claimed: false,
            },
        );
        Ok(token)
    }

    /// Claim a download slot. A manifest-issued grant can cover multiple Range
    /// requests after its first claim; a direct URL without a grant is one
    /// independent request. This keeps iOS resume behavior working without
    /// letting Range requests bypass the quota.
    ///
    /// iOS obtains this grant from its manifest; Android obtains it from the
    /// install page. The grant table lock is held across the slot claim so that marking a
    /// grant claimed and spending its slot cannot interleave.
    pub async fn authorize_download(
        &self,
        artifact_id: &str,
        grant_token: Option<&str>,
    ) -> Result<&Artifact, ServiceError> {
        self.match_id(artifact_id)?;
        if self.expires_at.is_some_and(|at| Instant::now() >= at) {
            return Err(ServiceError::Gone("this share link has expired".into()));
        }

        let mut grants = self.download_grants.lock().await;
        let now = Instant::now();
        grants.retain(|_, grant| grant.expires_at > now);
        let resuming = match grant_token {
            Some(token) => {
                grants
                    .get(token)
                    .ok_or_else(|| ServiceError::Forbidden("invalid download grant".into()))?
                    .claimed
            }
            None => false,
        };
        if !resuming {
            self.claim_slot()?;
            if let Some(token) = grant_token
                && let Some(grant) = grants.get_mut(token)
            {
                grant.claimed = true;
            }
        }
        drop(grants);

        tracing::info!(
            artifact_id = %artifact_id,
            download_count = self.download_count.load(Ordering::Relaxed),
            max_downloads = ?self.max_downloads,
            has_grant = grant_token.is_some(),
            resuming,
            "package download authorized"
        );
        Ok(&self.artifact)
    }

    /// Spend one OTA download slot, or report the link as exhausted. Fires the
    /// shutdown signal when that was the last slot.
    fn claim_slot(&self) -> Result<u64, ServiceError> {
        let Some(maximum) = self.max_downloads else {
            return Ok(self.download_count.fetch_add(1, Ordering::Relaxed) + 1);
        };
        let mut current = self.download_count.load(Ordering::Relaxed);
        let claimed = loop {
            if current >= maximum {
                return Err(ServiceError::Gone(
                    "this share link has reached its download limit".into(),
                ));
            }
            match self.download_count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break current + 1,
                Err(actual) => current = actual,
            }
        };
        if claimed >= maximum {
            // `notify_one` leaves a permit behind when nobody is waiting yet,
            // so the signal cannot be lost to a race with `share`'s startup.
            self.quota_spent.notify_one();
        }
        Ok(claimed)
    }

    /// Resolve once the share can no longer produce an install, so the caller
    /// can close the tunnel instead of leaving up a link that only answers
    /// 410. Never resolves when the share has neither a TTL nor a quota.
    pub async fn wait_until_unavailable(&self) -> Availability {
        let expiry = async {
            match self.expires_at {
                Some(at) => {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
                    Availability::Expired
                }
                None => std::future::pending().await,
            }
        };
        let quota = async {
            if self.max_downloads.is_none() {
                return std::future::pending().await;
            }
            self.quota_spent.notified().await;
            Availability::LimitReached
        };
        tokio::select! {
            reason = expiry => reason,
            reason = quota => reason,
        }
    }

    /// Resolve an artifact to the local file managed by this share session.
    pub fn download_path(&self, artifact_id: &str) -> Result<&Path, ServiceError> {
        self.match_id(artifact_id)?;
        Ok(&self.package_path)
    }

    pub fn icon_path(&self, artifact_id: &str) -> Result<&Path, ServiceError> {
        self.match_id(artifact_id)?;
        self.icon_path
            .as_deref()
            .ok_or_else(|| ServiceError::NotFound("artifact has no standalone PNG icon".into()))
    }

    pub async fn artifact_download_url(&self, artifact: &Artifact) -> Result<String, ServiceError> {
        let grant = self.issue_download_grant(&artifact.id).await?;
        Ok(format!(
            "{}?download={grant}",
            self.local_route(&format!(
                "/api/v1/artifacts/{}/download.{}",
                artifact.id,
                artifact.download_extension()
            ))
        ))
    }

    pub fn manifest_url(&self, artifact: &Artifact) -> String {
        self.local_route(&format!("/api/v1/artifacts/{}/manifest.plist", artifact.id))
    }

    pub fn install_page_url(&self, artifact: &Artifact) -> String {
        self.local_route(&format!("/install/{}", artifact.id))
    }

    pub fn icon_url(&self, artifact: &Artifact) -> Option<String> {
        artifact
            .has_icon
            .then(|| self.local_route(&format!("/api/v1/artifacts/{}/icon.png", artifact.id)))
    }

    pub async fn install_action_url(&self, artifact: &Artifact) -> Result<String, ServiceError> {
        match artifact.platform() {
            Platform::Ios => Ok(ipa::itms_services_url(&self.manifest_url(artifact))),
            Platform::Android => self.artifact_download_url(artifact).await,
        }
    }

    pub async fn manifest(&self, artifact: &Artifact) -> Result<String, ServiceError> {
        let PlatformMetadata::Ios(metadata) = &artifact.platform_metadata else {
            return Err(ServiceError::NotFound(
                "Android packages do not use an OTA manifest".into(),
            ));
        };
        let download_url = self.artifact_download_url(artifact).await?;
        let icon_url = self.icon_url(artifact);
        // iOS renders the home-screen placeholder from these two assets while
        // the package downloads; without them the user stares at a grey tile.
        let assets = ManifestAssets {
            ipa_url: &download_url,
            display_image_url: icon_url.as_deref(),
            full_size_image_url: icon_url.as_deref(),
        };
        Ok(ipa::manifest_xml(
            &metadata.bundle_identifier,
            &metadata.bundle_version,
            artifact.title(),
            &assets,
        ))
    }

    fn local_route(&self, path: &str) -> String {
        format!(
            "{}{}",
            self.public_base_url.as_str().trim_end_matches('/'),
            path
        )
    }
}

/// A deadline far enough out to be indistinguishable from "never", for a TTL
/// so large that adding it to now overflows the monotonic clock.
fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(60 * 60 * 24 * 365)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact_input;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes.extend_from_slice(&[0; 4]);
        bytes
    }

    fn example_ipa(path: &Path) {
        let mut writer = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
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

    async fn make_service(root: &Path, config: ShareConfig) -> ShareService {
        let source = root.join("Example.ipa");
        if !source.exists() {
            example_ipa(&source);
        }
        let staging = root.join("staging");
        let prepared = artifact_input::prepare(
            &source,
            None,
            &staging,
            artifact_input::SigningPolicy::Trusted,
        )
        .unwrap();
        ShareService::create(
            root.join(format!("workspace-{}", uuid::Uuid::new_v4())),
            Url::parse("https://installer.example.test").unwrap(),
            &prepared,
            config,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn create_stages_the_ipa_and_its_icon_beside_each_other() {
        let temporary = tempfile::tempdir().unwrap();
        let service = make_service(temporary.path(), ShareConfig::default()).await;
        let artifact = service.artifact().clone();

        assert!(artifact.has_icon);
        let PlatformMetadata::Ios(metadata) = &artifact.platform_metadata else {
            panic!("fixture should produce an iOS artifact");
        };
        assert_eq!(metadata.minimum_os_version.as_deref(), Some("16.0"));
        assert_eq!(
            service.icon_url(&artifact).as_deref(),
            Some(
                format!(
                    "https://installer.example.test/api/v1/artifacts/{}/icon.png",
                    artifact.id
                )
                .as_str()
            )
        );
        assert!(service.download_path(&artifact.id).unwrap().is_file());
        assert_eq!(
            std::fs::read(service.icon_path(&artifact.id).unwrap()).unwrap(),
            png(180, 180)
        );
    }

    #[tokio::test]
    async fn unknown_artifact_ids_are_not_found_on_every_route() {
        let temporary = tempfile::tempdir().unwrap();
        let service = make_service(temporary.path(), ShareConfig::default()).await;

        assert!(matches!(
            service.viewable_artifact("artifact-nope"),
            Err(ServiceError::NotFound(_))
        ));
        assert!(matches!(
            service.servable_artifact("artifact-nope"),
            Err(ServiceError::NotFound(_))
        ));
        assert!(matches!(
            service.download_path("artifact-nope"),
            Err(ServiceError::NotFound(_))
        ));
        assert!(matches!(
            service.authorize_download("artifact-nope", None).await,
            Err(ServiceError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn manifest_advertises_the_icon_assets() {
        let temporary = tempfile::tempdir().unwrap();
        let service = make_service(temporary.path(), ShareConfig::default()).await;

        let manifest = service.manifest(service.artifact()).await.unwrap();
        assert!(manifest.contains("display-image"));
        assert!(manifest.contains("full-size-image"));
        assert!(manifest.contains("software-package"));
    }

    #[tokio::test]
    async fn a_spent_quota_stops_installs_but_leaves_the_page_and_icon_readable() {
        let temporary = tempfile::tempdir().unwrap();
        let service = make_service(
            temporary.path(),
            ShareConfig {
                max_downloads: Some(1),
                ..ShareConfig::default()
            },
        )
        .await;
        let id = service.artifact().id.clone();

        assert_eq!(service.availability(), Availability::Installable);
        service.authorize_download(&id, None).await.unwrap();
        assert_eq!(service.availability(), Availability::LimitReached);

        // The install routes close...
        assert!(matches!(
            service.servable_artifact(&id),
            Err(ServiceError::Gone(_))
        ));
        assert!(matches!(
            service.issue_download_grant(&id).await,
            Err(ServiceError::Gone(_))
        ));
        // ...while the presentation routes stay open, so a device that failed
        // to install can still be told why.
        assert!(service.viewable_artifact(&id).is_ok());
        assert!(service.icon_path(&id).is_ok());
    }

    #[tokio::test]
    async fn an_expired_share_closes_every_install_route() {
        let temporary = tempfile::tempdir().unwrap();
        let service = make_service(
            temporary.path(),
            ShareConfig {
                artifact_ttl: Some(Duration::ZERO),
                ..ShareConfig::default()
            },
        )
        .await;
        let id = service.artifact().id.clone();

        assert_eq!(service.availability(), Availability::Expired);
        assert!(matches!(
            service.servable_artifact(&id),
            Err(ServiceError::Gone(_))
        ));
        assert!(matches!(
            service.authorize_download(&id, None).await,
            Err(ServiceError::Gone(_))
        ));
        assert!(service.viewable_artifact(&id).is_ok());
    }

    #[tokio::test]
    async fn a_claimed_grant_covers_retries_while_a_bare_request_spends_a_slot() {
        let temporary = tempfile::tempdir().unwrap();
        let service = make_service(
            temporary.path(),
            ShareConfig {
                max_downloads: Some(2),
                ..ShareConfig::default()
            },
        )
        .await;
        let id = service.artifact().id.clone();

        let grant = service.issue_download_grant(&id).await.unwrap();
        for _ in 0..5 {
            service
                .authorize_download(&id, Some(&grant))
                .await
                .expect("a claimed grant covers the whole attempt");
        }
        assert_eq!(service.download_count.load(Ordering::Relaxed), 1);

        service.authorize_download(&id, None).await.unwrap();
        assert_eq!(service.download_count.load(Ordering::Relaxed), 2);
        assert!(matches!(
            service.authorize_download(&id, None).await,
            Err(ServiceError::Gone(_))
        ));
    }

    #[tokio::test]
    async fn an_unknown_grant_is_rejected_rather_than_silently_counted() {
        let temporary = tempfile::tempdir().unwrap();
        let service = make_service(temporary.path(), ShareConfig::default()).await;
        let id = service.artifact().id.clone();

        assert!(matches!(
            service.authorize_download(&id, Some("not-a-grant")).await,
            Err(ServiceError::Forbidden(_))
        ));
        assert_eq!(service.download_count.load(Ordering::Relaxed), 0);
    }

    /// Anyone holding the link can reload the manifest; that must never be
    /// able to refuse a grant to the device actually trying to install.
    #[tokio::test]
    async fn minting_grants_forever_evicts_the_oldest_instead_of_failing() {
        let temporary = tempfile::tempdir().unwrap();
        let service = make_service(temporary.path(), ShareConfig::default()).await;
        let id = service.artifact().id.clone();

        let mut newest = String::new();
        for _ in 0..(MAX_DOWNLOAD_GRANTS * 8) {
            newest = service
                .issue_download_grant(&id)
                .await
                .expect("issuing a grant must never fail on a live share");
        }
        assert_eq!(
            service.download_grants.lock().await.len(),
            MAX_DOWNLOAD_GRANTS
        );
        service
            .authorize_download(&id, Some(&newest))
            .await
            .expect("the most recent grant survives eviction");
    }

    #[tokio::test]
    async fn wait_until_unavailable_reports_why_the_share_ended() {
        let temporary = tempfile::tempdir().unwrap();

        let expiring = make_service(
            temporary.path(),
            ShareConfig {
                artifact_ttl: Some(Duration::ZERO),
                ..ShareConfig::default()
            },
        )
        .await;
        assert_eq!(
            expiring.wait_until_unavailable().await,
            Availability::Expired
        );

        let limited = make_service(
            temporary.path(),
            ShareConfig {
                max_downloads: Some(1),
                ..ShareConfig::default()
            },
        )
        .await;
        let id = limited.artifact().id.clone();
        limited.authorize_download(&id, None).await.unwrap();
        assert_eq!(
            limited.wait_until_unavailable().await,
            Availability::LimitReached
        );

        // With neither limit configured the share runs until Ctrl-C.
        let unlimited = make_service(temporary.path(), ShareConfig::default()).await;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                unlimited.wait_until_unavailable()
            )
            .await
            .is_err()
        );
    }
}
