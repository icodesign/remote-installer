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

/// Immutable description of the single artifact a share session serves.
///
/// Session-scoped limits — expiry, the download quota, how much of it is
/// spent — deliberately live on `ShareService` instead of here. Everything in
/// this struct is fixed the moment the IPA is inspected, which is what lets
/// the service hand out `&Artifact` without a lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub id: String,
    pub file_name: String,
    pub byte_count: u64,
    pub sha256: String,
    pub bundle_identifier: String,
    pub bundle_version: String,
    pub bundle_short_version: Option<String>,
    pub display_name: Option<String>,
    /// `MinimumOSVersion` from the app's `Info.plist`, when it declares one.
    pub minimum_os_version: Option<String>,
    /// Whether a standalone PNG icon was extracted alongside the IPA.
    pub has_icon: bool,
}

impl Artifact {
    /// The name to show a human: the app's own display name when it declares
    /// a usable one, otherwise the IPA file name.
    pub fn title(&self) -> &str {
        self.display_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.file_name)
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
            bundle_identifier: "com.example.app".into(),
            bundle_version: "7".into(),
            bundle_short_version: None,
            display_name: display_name.map(ToOwned::to_owned),
            minimum_os_version: None,
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
}
