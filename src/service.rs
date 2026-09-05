use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
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
/// Do not let a client create an unbounded number of disjoint ranges on one
/// grant. Normal iOS resume traffic uses only a handful of ranges; once this
/// bound is reached, a later full-file request can still complete the grant.
const MAX_TRACKED_DOWNLOAD_RANGES: usize = 4096;
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
    #[error("invalid share configuration: {0}")]
    InvalidConfiguration(String),
    #[error("a download is already in progress")]
    DownloadInProgress,
}

/// Per-share limits. A share session is intentionally ephemeral: the
/// artifact and its bytes disappear when the command exits.
#[derive(Debug, Clone, Default)]
pub struct ShareConfig {
    /// How long the share remains installable.
    pub artifact_ttl: Option<Duration>,
    /// How many successful OTA download attempts the share allows. A
    /// manifest-issued grant may cover multiple Range requests for one
    /// attempt.
    pub max_downloads: Option<u64>,
}

#[derive(Debug, Default)]
struct DownloadCounts {
    completed: u64,
    reserved: u64,
}

/// The quota is a small synchronous state machine because completion and
/// cancellation happen from a response body's `Drop` implementation, which
/// cannot await a Tokio mutex. Keeping completed and reserved downloads under
/// one lock makes the capacity check atomic: a completion cannot race a new
/// reservation and let more than `max_downloads` successful attempts through.
pub(crate) struct DownloadQuota {
    max_downloads: Option<u64>,
    counts: StdMutex<DownloadCounts>,
    quota_spent: Notify,
}

impl DownloadQuota {
    pub(crate) fn new(max_downloads: Option<u64>) -> Self {
        let quota = Self {
            max_downloads,
            counts: StdMutex::new(DownloadCounts::default()),
            quota_spent: Notify::new(),
        };
        if max_downloads == Some(0) {
            quota.quota_spent.notify_one();
        }
        quota
    }

    fn counts(&self) -> std::sync::MutexGuard<'_, DownloadCounts> {
        self.counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn completed(&self) -> u64 {
        self.counts().completed
    }

    /// Reserve one eventual successful download. A reservation is not part
    /// of the public count; it only prevents concurrent attempts from
    /// oversubscribing the configured maximum.
    pub(crate) fn reserve(&self) -> Result<(), ServiceError> {
        let mut counts = self.counts();
        if let Some(maximum) = self.max_downloads {
            if counts.completed >= maximum {
                return Err(ServiceError::Gone(
                    "this share link has reached its download limit".into(),
                ));
            }
            if counts.completed.saturating_add(counts.reserved) >= maximum {
                return Err(ServiceError::DownloadInProgress);
            }
        }
        counts.reserved = counts.reserved.saturating_add(1);
        Ok(())
    }

    fn complete(&self) {
        let spent = {
            let mut counts = self.counts();
            debug_assert!(counts.reserved > 0);
            counts.reserved = counts.reserved.saturating_sub(1);
            counts.completed = counts.completed.saturating_add(1);
            self.max_downloads
                .is_some_and(|maximum| counts.completed >= maximum)
        };
        if spent {
            // `notify_one` leaves a permit behind when nobody is waiting yet,
            // so the share shutdown watcher cannot miss the transition.
            self.quota_spent.notify_one();
        }
    }

    pub(crate) fn release(&self) {
        let mut counts = self.counts();
        debug_assert!(counts.reserved > 0);
        counts.reserved = counts.reserved.saturating_sub(1);
    }

    fn is_spent(&self) -> bool {
        self.max_downloads
            .is_some_and(|maximum| self.completed() >= maximum)
    }
}

/// State shared by every request that uses one manifest-issued grant.
/// `active_requests` allows Range retries to overlap without reserving extra
/// download slots, while `covered_ranges` lets the service distinguish a
/// successful partial response from a complete package transfer.
struct DownloadGrantState {
    status: StdMutex<DownloadGrantStatus>,
}

#[derive(Debug, Default)]
struct DownloadGrantStatus {
    active_requests: u64,
    completed: bool,
    reserved: bool,
    covered_ranges: Vec<(u64, u64)>,
}

/// A body-owned reservation for one direct download or one manifest grant.
/// Dropping it before the response body reaches EOF releases the reservation
/// without incrementing `max_downloads`.
pub(crate) struct DownloadPermit {
    quota: Arc<DownloadQuota>,
    grant: Option<Arc<DownloadGrantState>>,
    active: bool,
    configured_range: Option<(u64, u64)>,
    total_length: Option<u64>,
    finished: bool,
}

impl DownloadPermit {
    pub(crate) fn direct(quota: Arc<DownloadQuota>) -> Self {
        Self {
            quota,
            grant: None,
            active: true,
            configured_range: None,
            total_length: None,
            finished: false,
        }
    }

    fn for_grant(quota: Arc<DownloadQuota>, grant: Arc<DownloadGrantState>, active: bool) -> Self {
        Self {
            quota,
            grant: Some(grant),
            active,
            configured_range: None,
            total_length: None,
            finished: false,
        }
    }

    /// Configure the response shape once the file has been opened and its
    /// Range header validated. An error before this point drops the permit and
    /// therefore releases the reservation.
    pub(crate) fn configure_response(&mut self, range: Option<(u64, u64)>, total: u64) {
        self.configured_range = range;
        self.total_length = Some(total);
    }

    /// Commit the reservation after the response body has delivered every
    /// byte it promised. A grant commits only after a full file was delivered;
    /// disjoint Range responses accumulate coverage across retries.
    pub(crate) fn complete_response(&mut self) {
        if self.finished {
            return;
        }
        self.finished = true;
        if !self.active {
            return;
        }

        let Some(grant) = self.grant.as_ref() else {
            self.active = false;
            self.quota.complete();
            return;
        };

        let (should_complete, should_release) = {
            let mut status = grant
                .status
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            status.active_requests = status.active_requests.saturating_sub(1);
            if status.completed {
                (false, false)
            } else {
                let complete = match (self.configured_range, self.total_length) {
                    (None, Some(_)) => true,
                    (Some((start, end)), Some(total)) => {
                        add_download_range(&mut status.covered_ranges, start, end);
                        covers_entire_download(&status.covered_ranges, total)
                    }
                    _ => false,
                };
                let release = !complete && status.active_requests == 0 && status.reserved;
                if complete {
                    status.completed = true;
                    status.reserved = false;
                } else if release {
                    // The response itself succeeded, but it was only one
                    // piece of a resumable transfer. Let another download
                    // attempt use the slot while this grant waits for its
                    // next Range request.
                    status.reserved = false;
                }
                (complete, release)
            }
        };
        self.active = false;

        if should_complete {
            self.quota.complete();
        } else if should_release {
            self.quota.release();
        }
    }
}

impl Drop for DownloadPermit {
    fn drop(&mut self) {
        if self.finished || !self.active {
            return;
        }
        self.active = false;

        let should_release = match self.grant.as_ref() {
            None => true,
            Some(grant) => {
                let mut status = grant
                    .status
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                status.active_requests = status.active_requests.saturating_sub(1);
                if status.active_requests == 0 && !status.completed && status.reserved {
                    status.reserved = false;
                    true
                } else {
                    false
                }
            }
        };
        if should_release {
            self.quota.release();
        }
    }
}

fn add_download_range(ranges: &mut Vec<(u64, u64)>, start: u64, end: u64) {
    if start > end {
        return;
    }
    ranges.push((start, end));
    ranges.sort_unstable_by_key(|(range_start, _)| *range_start);
    let mut merged = Vec::with_capacity(ranges.len());
    for (range_start, range_end) in ranges.drain(..) {
        let Some((_, previous_end)) = merged.last_mut() else {
            merged.push((range_start, range_end));
            continue;
        };
        if range_start <= previous_end.saturating_add(1) {
            *previous_end = (*previous_end).max(range_end);
        } else if merged.len() < MAX_TRACKED_DOWNLOAD_RANGES {
            merged.push((range_start, range_end));
        }
    }
    *ranges = merged;
}

fn covers_entire_download(ranges: &[(u64, u64)], total: u64) -> bool {
    total == 0
        || ranges
            .first()
            .is_some_and(|(start, end)| *start == 0 && end.saturating_add(1) >= total)
}

/// Runtime state for one `remote-installer share` session.
///
/// The artifact itself is immutable. Mutable download state is kept in small,
/// explicit quota and grant objects: public requests reserve capacity, the
/// response body commits successful transfers, and dropping that body releases
/// failed attempts. Public downloads always pass through this service, so
/// there is no second URL or credential-bearing storage path for a client to
/// bypass.
pub struct ShareService {
    /// Public origins currently exposing this share. The first one is the
    /// default used by callers that do not have an incoming Host header (for
    /// example the terminal banner); HTTP requests select the matching origin
    /// so pages reached through a second provider keep their own install URLs.
    public_base_urls: Vec<Url>,
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
    quota: Arc<DownloadQuota>,
    /// Grants are issued when a manifest is requested. A grant stays valid for
    /// Range retries belonging to that same OTA attempt, including retries
    /// after an interrupted response.
    download_grants: Mutex<HashMap<String, DownloadGrant>>,
}

struct DownloadGrant {
    issued_at: Instant,
    expires_at: Instant,
    state: Arc<DownloadGrantState>,
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
        Self::create_with_public_base_urls(workspace_dir, vec![public_base_url], prepared, config)
            .await
    }

    /// Stage a prepared artifact for one or more provider origins.
    ///
    /// All origins share one artifact, quota, and grant table. A provider is
    /// only an alternate route to the same ephemeral share; duplicating the
    /// service here would let the same phone consume one download slot per
    /// tunnel instead of one slot per completed transfer.
    pub async fn create_with_public_base_urls(
        workspace_dir: impl Into<PathBuf>,
        public_base_urls: Vec<Url>,
        prepared: &PreparedArtifact,
        config: ShareConfig,
    ) -> Result<Self, ServiceError> {
        if public_base_urls.is_empty() {
            return Err(ServiceError::InvalidConfiguration(
                "at least one public provider URL is required".into(),
            ));
        }
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
            public_base_urls,
            artifact,
            package_path,
            icon_path,
            expires_at: config
                .artifact_ttl
                .map(|ttl| Instant::now().checked_add(ttl).unwrap_or_else(far_future)),
            quota: Arc::new(DownloadQuota::new(config.max_downloads)),
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

    /// The provider origins in the order supplied to `create_with_public_base_urls`.
    pub fn public_base_urls(&self) -> &[Url] {
        &self.public_base_urls
    }

    /// Choose the public origin represented by an incoming Host header.
    /// Unknown hosts fall back to the first configured origin; the origin
    /// server is loopback-only, and a tunnel must already be configured for a
    /// URL to reach it, so this is a URL-generation choice rather than an
    /// access-control decision.
    pub fn public_base_url_for_authority(&self, authority: Option<&str>) -> &Url {
        authority
            .and_then(|authority| {
                self.public_base_urls
                    .iter()
                    .find(|base| authority_matches(base, authority))
            })
            .unwrap_or(&self.public_base_urls[0])
    }

    pub fn availability(&self) -> Availability {
        if self.expires_at.is_some_and(|at| Instant::now() >= at) {
            return Availability::Expired;
        }
        if self.quota.is_spent() {
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

    /// Issue an opaque, short-lived grant for the next package download. The
    /// grant is intentionally not counted until its response completes:
    /// opening a manifest or starting an interrupted transfer must not consume
    /// a download slot.
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
                state: Arc::new(DownloadGrantState {
                    status: StdMutex::new(DownloadGrantStatus::default()),
                }),
            },
        );
        Ok(token)
    }

    /// Reserve a download slot. The returned permit must stay attached to the
    /// response body: it commits only after the promised bytes have all been
    /// delivered, and releases the reservation if that body is interrupted.
    ///
    /// A manifest-issued grant can cover multiple Range requests for one OTA
    /// attempt; a direct URL without a grant is one independent request. This
    /// keeps iOS resume behavior working without letting Range requests bypass
    /// the quota.
    ///
    /// iOS obtains this grant from its manifest; Android obtains it from the
    /// install page. The grant table lock is held only long enough to resolve
    /// the grant state; its own status lock then serializes overlapping retries.
    pub(crate) async fn authorize_download(
        &self,
        artifact_id: &str,
        grant_token: Option<&str>,
    ) -> Result<DownloadPermit, ServiceError> {
        self.match_id(artifact_id)?;
        if self.expires_at.is_some_and(|at| Instant::now() >= at) {
            return Err(ServiceError::Gone("this share link has expired".into()));
        }

        let grant = if let Some(token) = grant_token {
            let mut grants = self.download_grants.lock().await;
            let now = Instant::now();
            grants.retain(|_, grant| grant.expires_at > now);
            Some(
                grants
                    .get(token)
                    .ok_or_else(|| ServiceError::Forbidden("invalid download grant".into()))?
                    .state
                    .clone(),
            )
        } else {
            None
        };

        let (permit, resuming) = match grant {
            None => {
                self.quota.reserve()?;
                (DownloadPermit::direct(Arc::clone(&self.quota)), false)
            }
            Some(grant) => {
                let (resuming, active) = {
                    let mut status = grant
                        .status
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let resuming = status.completed || status.active_requests > 0;
                    if status.completed {
                        (resuming, false)
                    } else {
                        status.active_requests = status.active_requests.saturating_add(1);
                        if !status.reserved {
                            if let Err(error) = self.quota.reserve() {
                                status.active_requests = status.active_requests.saturating_sub(1);
                                return Err(error);
                            }
                            status.reserved = true;
                        }
                        (resuming, true)
                    }
                };
                if !active {
                    // A completed grant remains usable for retries belonging
                    // to that same OTA attempt, but it cannot increment the
                    // quota a second time.
                    (
                        DownloadPermit::for_grant(Arc::clone(&self.quota), grant, false),
                        resuming,
                    )
                } else {
                    (
                        DownloadPermit::for_grant(Arc::clone(&self.quota), grant, true),
                        resuming,
                    )
                }
            }
        };

        tracing::info!(
            artifact_id = %artifact_id,
            download_count = self.quota.completed(),
            max_downloads = ?self.quota.max_downloads,
            has_grant = grant_token.is_some(),
            resuming,
            "package download authorized"
        );
        Ok(permit)
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
            if self.quota.max_downloads.is_none() {
                return std::future::pending().await;
            }
            if self.quota.is_spent() {
                return Availability::LimitReached;
            }
            self.quota.quota_spent.notified().await;
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
        self.artifact_download_url_at(artifact, &self.public_base_urls[0])
            .await
    }

    pub async fn artifact_download_url_at(
        &self,
        artifact: &Artifact,
        public_base_url: &Url,
    ) -> Result<String, ServiceError> {
        let grant = self.issue_download_grant(&artifact.id).await?;
        Ok(format!(
            "{}?download={grant}",
            self.local_route_at(
                public_base_url,
                &format!(
                    "/api/v1/artifacts/{}/download.{}",
                    artifact.id,
                    artifact.download_extension()
                )
            )
        ))
    }

    pub fn manifest_url(&self, artifact: &Artifact) -> String {
        self.manifest_url_at(artifact, &self.public_base_urls[0])
    }

    pub fn manifest_url_at(&self, artifact: &Artifact, public_base_url: &Url) -> String {
        self.local_route_at(
            public_base_url,
            &format!("/api/v1/artifacts/{}/manifest.plist", artifact.id),
        )
    }

    pub fn install_page_url(&self, artifact: &Artifact) -> String {
        self.install_page_url_at(artifact, &self.public_base_urls[0])
    }

    pub fn install_page_url_at(&self, artifact: &Artifact, public_base_url: &Url) -> String {
        self.local_route_at(public_base_url, &format!("/install/{}", artifact.id))
    }

    pub fn icon_url(&self, artifact: &Artifact) -> Option<String> {
        self.icon_url_at(artifact, &self.public_base_urls[0])
    }

    pub fn icon_url_at(&self, artifact: &Artifact, public_base_url: &Url) -> Option<String> {
        artifact.has_icon.then(|| {
            self.local_route_at(
                public_base_url,
                &format!("/api/v1/artifacts/{}/icon.png", artifact.id),
            )
        })
    }

    pub async fn install_action_url(&self, artifact: &Artifact) -> Result<String, ServiceError> {
        self.install_action_url_at(artifact, &self.public_base_urls[0])
            .await
    }

    pub async fn install_action_url_at(
        &self,
        artifact: &Artifact,
        public_base_url: &Url,
    ) -> Result<String, ServiceError> {
        match artifact.platform() {
            Platform::Ios => Ok(ipa::itms_services_url(
                &self.manifest_url_at(artifact, public_base_url),
            )),
            Platform::Android => {
                self.artifact_download_url_at(artifact, public_base_url)
                    .await
            }
        }
    }

    pub async fn manifest(&self, artifact: &Artifact) -> Result<String, ServiceError> {
        self.manifest_at(artifact, &self.public_base_urls[0]).await
    }

    pub async fn manifest_at(
        &self,
        artifact: &Artifact,
        public_base_url: &Url,
    ) -> Result<String, ServiceError> {
        let PlatformMetadata::Ios(metadata) = &artifact.platform_metadata else {
            return Err(ServiceError::NotFound(
                "Android packages do not use an OTA manifest".into(),
            ));
        };
        let download_url = self
            .artifact_download_url_at(artifact, public_base_url)
            .await?;
        let icon_url = self.icon_url_at(artifact, public_base_url);
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

    fn local_route_at(&self, public_base_url: &Url, path: &str) -> String {
        format!("{}{}", public_base_url.as_str().trim_end_matches('/'), path)
    }
}

/// A deadline far enough out to be indistinguishable from "never", for a TTL
/// so large that adding it to now overflows the monotonic clock.
fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(60 * 60 * 24 * 365)
}

fn authority_matches(base: &Url, authority: &str) -> bool {
    let Ok(candidate) = Url::parse(&format!("http://{authority}/")) else {
        return false;
    };
    let Some(base_host) = base.host_str() else {
        return false;
    };
    let Some(candidate_host) = candidate.host_str() else {
        return false;
    };
    if !base_host.eq_ignore_ascii_case(candidate_host) {
        return false;
    }
    let base_port = base.port_or_known_default();
    let candidate_port = candidate.port_or_known_default();
    base_port == candidate_port
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
        make_service_with_urls(
            root,
            config,
            vec![Url::parse("https://installer.example.test").unwrap()],
        )
        .await
    }

    async fn make_service_with_urls(
        root: &Path,
        config: ShareConfig,
        public_base_urls: Vec<Url>,
    ) -> ShareService {
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
        ShareService::create_with_public_base_urls(
            root.join(format!("workspace-{}", uuid::Uuid::new_v4())),
            public_base_urls,
            &prepared,
            config,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn selects_the_provider_origin_for_host_based_urls() {
        let temporary = tempfile::tempdir().unwrap();
        let service = make_service_with_urls(
            temporary.path(),
            ShareConfig::default(),
            vec![
                Url::parse("https://tailnet.example.test").unwrap(),
                Url::parse("https://public.example.test:8443").unwrap(),
            ],
        )
        .await;
        let artifact = service.artifact().clone();

        assert_eq!(
            service
                .public_base_url_for_authority(Some("public.example.test:8443"))
                .as_str(),
            "https://public.example.test:8443/"
        );
        assert_eq!(
            service
                .public_base_url_for_authority(Some("PUBLIC.EXAMPLE.TEST:8443"))
                .as_str(),
            "https://public.example.test:8443/"
        );
        assert_eq!(
            service.install_page_url_at(
                &artifact,
                service.public_base_url_for_authority(Some("public.example.test:8443"))
            ),
            format!("https://public.example.test:8443/install/{}", artifact.id)
        );
        assert_eq!(
            service
                .public_base_url_for_authority(Some("unknown.example.test"))
                .as_str(),
            "https://tailnet.example.test/"
        );
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
        let mut permit = service.authorize_download(&id, None).await.unwrap();
        permit.configure_response(None, 1);
        permit.complete_response();
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
    async fn a_grant_counts_only_after_its_full_transfer_is_complete() {
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
        for range in [(0, 1), (2, 3), (0, 3), (1, 2), (0, 0)] {
            let mut permit = service
                .authorize_download(&id, Some(&grant))
                .await
                .expect("a grant covers the whole attempt");
            permit.configure_response(Some(range), 4);
            permit.complete_response();
        }
        assert_eq!(service.quota.completed(), 1);

        let mut permit = service.authorize_download(&id, None).await.unwrap();
        permit.configure_response(None, 1);
        permit.complete_response();
        assert_eq!(service.quota.completed(), 2);
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
        assert_eq!(service.quota.completed(), 0);
    }

    #[tokio::test]
    async fn an_interrupted_download_releases_its_reservation_without_counting() {
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

        let permit = service.authorize_download(&id, None).await.unwrap();
        assert_eq!(service.quota.completed(), 0);
        assert_eq!(service.availability(), Availability::Installable);
        drop(permit);

        let mut permit = service.authorize_download(&id, None).await.unwrap();
        permit.configure_response(None, 1);
        permit.complete_response();
        assert_eq!(service.quota.completed(), 1);
        assert_eq!(service.availability(), Availability::LimitReached);
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
        let mut permit = limited.authorize_download(&id, None).await.unwrap();
        permit.configure_response(None, 1);
        permit.complete_response();
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
