pub const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Whether the share can still produce an install right now.
///
/// Lives here rather than beside `ShareService` so the install page can render
/// an honest "why not" without depending on the service layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Installable,
    /// `--max-downloads` is spent.
    LimitReached,
    /// `--expire-after` has elapsed.
    Expired,
}

/// Metadata whose meaning is defined by the target operating system.
///
/// Keeping this as an enum prevents the shared service and page from treating
/// an Android package name as an iOS bundle identifier, or an Android API
/// level as an iOS version string. Common file metadata stays on `Artifact`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformMetadata {
    Ios(IosMetadata),
    Android(AndroidMetadata),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IosMetadata {
    pub bundle_identifier: String,
    pub bundle_version: String,
    pub bundle_short_version: Option<String>,
    /// `MinimumOSVersion` from the app's `Info.plist`, when declared.
    pub minimum_os_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidMetadata {
    /// Absent when `apkanalyzer` was unavailable during preparation.
    pub package_name: Option<String>,
    /// Absent when `apkanalyzer` was unavailable during preparation.
    pub version_code: Option<String>,
    pub version_name: Option<String>,
    pub min_sdk: Option<u32>,
    pub target_sdk: Option<u32>,
    /// SHA-256 digest of the APK signing certificate, when `apksigner` ran.
    pub certificate_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Ios,
    Android,
}

/// Immutable description of the single artifact a share session serves.
///
/// Session-scoped limits — expiry, the download quota, how much of it is
/// spent — deliberately live on `ShareService` instead of here. Everything in
/// this struct is fixed the moment the package is inspected, which is what lets
/// the service hand out `&Artifact` without a lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub id: String,
    pub file_name: String,
    pub byte_count: u64,
    pub sha256: String,
    pub display_name: Option<String>,
    pub platform_metadata: PlatformMetadata,
    /// Whether a standalone PNG icon was extracted alongside the package.
    pub has_icon: bool,
}

impl Artifact {
    /// The name to show a human: the app's own display name when it declares
    /// a usable one, otherwise the package file name.
    pub fn title(&self) -> &str {
        self.display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.file_name)
    }

    pub fn platform(&self) -> Platform {
        match self.platform_metadata {
            PlatformMetadata::Ios(_) => Platform::Ios,
            PlatformMetadata::Android(_) => Platform::Android,
        }
    }

    pub fn download_extension(&self) -> &'static str {
        match self.platform() {
            Platform::Ios => "ipa",
            Platform::Android => "apk",
        }
    }

    pub fn download_content_type(&self) -> &'static str {
        match self.platform() {
            Platform::Ios => "application/octet-stream",
            Platform::Android => "application/vnd.android.package-archive",
        }
    }
}

/// Render a byte count the way a phone would: decimal units, one decimal
/// place. Shared by the install page and the terminal's download progress so
/// the two never disagree about how big the same build is.
pub fn format_bytes(byte_count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = byte_count as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{byte_count} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(display_name: Option<&str>) -> Artifact {
        Artifact {
            id: "artifact-1".into(),
            file_name: "Example.ipa".into(),
            byte_count: 42,
            sha256: "abc123".into(),
            display_name: display_name.map(ToOwned::to_owned),
            platform_metadata: PlatformMetadata::Ios(IosMetadata {
                bundle_identifier: "com.example.app".into(),
                bundle_version: "7".into(),
                bundle_short_version: None,
                minimum_os_version: None,
            }),
            has_icon: false,
        }
    }

    #[test]
    fn byte_counts_render_in_decimal_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1_234), "1.2 KB");
        assert_eq!(format_bytes(214_600_000), "214.6 MB");
    }

    #[test]
    fn title_falls_back_to_the_file_name_when_no_usable_display_name_exists() {
        assert_eq!(artifact(Some("Example")).title(), "Example");
        assert_eq!(artifact(None).title(), "Example.ipa");
        assert_eq!(artifact(Some("   ")).title(), "Example.ipa");
    }

    #[test]
    fn platform_controls_download_transport_metadata() {
        let mut artifact = artifact(Some("Example"));
        assert_eq!(artifact.platform(), Platform::Ios);
        assert_eq!(artifact.download_extension(), "ipa");
        assert_eq!(artifact.download_content_type(), "application/octet-stream");

        artifact.platform_metadata = PlatformMetadata::Android(AndroidMetadata {
            package_name: Some("com.example.app".into()),
            version_code: Some("7".into()),
            version_name: Some("1.0".into()),
            min_sdk: Some(26),
            target_sdk: Some(36),
            certificate_sha256: Some("abc123".into()),
        });
        assert_eq!(artifact.platform(), Platform::Android);
        assert_eq!(artifact.download_extension(), "apk");
        assert_eq!(
            artifact.download_content_type(),
            "application/vnd.android.package-archive"
        );
    }
}
