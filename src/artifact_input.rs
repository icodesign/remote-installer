use std::ffi::OsStr;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::SystemTime;

use plist::Value;
use tempfile::TempDir;
use thiserror::Error;

use crate::apk::{self, ApkError, ApkToolchain};
use crate::ipa::{self, IpaError, IpaMetadata, SigningEvidence};
use crate::model::{AndroidMetadata, IosMetadata, PlatformMetadata};

#[derive(Debug, Error)]
pub enum ArtifactInputError {
    #[error("artifact input I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("app property list is invalid: {0}")]
    Plist(#[from] plist::Error),
    #[error(transparent)]
    Ipa(#[from] IpaError),
    #[error(transparent)]
    Apk(#[from] ApkError),
    #[error("artifact input is invalid: {0}")]
    Invalid(String),
    #[error("{tool} failed: {message}")]
    Tool { tool: &'static str, message: String },
}

/// What to require of an IPA's signing evidence before sharing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigningPolicy {
    /// Reject an IPA that iOS could not install anyway.
    Required,
    /// Share whatever the caller pointed at (`--allow-unsigned`).
    Trusted,
}

/// A canonical installable package ready for the share service to stage.
/// The temporary directory, when present, owns a package derived from an input
/// such as a local iOS app bundle.
pub struct PreparedArtifact {
    path: PathBuf,
    metadata: PreparedArtifactMetadata,
    warnings: Vec<String>,
    _temporary: Option<TempDir>,
}

#[derive(Debug, Clone)]
pub struct PreparedArtifactMetadata {
    pub file_name: String,
    pub byte_count: u64,
    pub sha256: String,
    pub display_name: Option<String>,
    pub platform_metadata: PlatformMetadata,
    pub icon_png: Option<Vec<u8>>,
}

impl PreparedArtifact {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn metadata(&self) -> &PreparedArtifactMetadata {
        &self.metadata
    }

    /// Non-fatal validation checks that could not be performed.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

#[derive(Debug)]
struct AppBundle {
    name: String,
    executable: PathBuf,
    bundle_identifier: String,
    provisioning_profile: PathBuf,
}

pub fn prepare(
    source: &Path,
    requested_file_name: Option<&str>,
    staging_root: &Path,
    signing_policy: SigningPolicy,
) -> Result<PreparedArtifact, ArtifactInputError> {
    let apk_toolchain = source
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| extension.eq_ignore_ascii_case("apk"))
        .then(|| ApkToolchain::discover(None, None))
        .transpose()?;
    prepare_with_apk_toolchain(
        source,
        requested_file_name,
        staging_root,
        signing_policy,
        apk_toolchain.as_ref(),
    )
}

/// Prepare either an iOS package or an Android APK.
///
/// Android inspection uses the official SDK tools when available. Keeping the
/// selected toolchain explicit here makes tests deterministic and prevents
/// platform tool discovery from leaking into the artifact/service layers.
pub fn prepare_with_apk_toolchain(
    source: &Path,
    requested_file_name: Option<&str>,
    staging_root: &Path,
    signing_policy: SigningPolicy,
    apk_toolchain: Option<&ApkToolchain>,
) -> Result<PreparedArtifact, ArtifactInputError> {
    let source_metadata = std::fs::symlink_metadata(source)?;
    if source_metadata.file_type().is_symlink() {
        return Err(ArtifactInputError::Invalid(
            "artifact source must not be a symbolic link".into(),
        ));
    }
    if source_metadata.is_file() {
        let extension = source
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase);
        if extension.as_deref() == Some("apk") {
            let unavailable_toolchain = ApkToolchain::from_optional_paths(None, None);
            let toolchain = apk_toolchain.unwrap_or(&unavailable_toolchain);
            let metadata = apk::inspect(source, requested_file_name, toolchain)?;
            return Ok(PreparedArtifact {
                path: source.to_path_buf(),
                metadata: PreparedArtifactMetadata {
                    file_name: metadata.file_name,
                    byte_count: metadata.byte_count,
                    sha256: metadata.sha256,
                    display_name: metadata.display_name,
                    platform_metadata: PlatformMetadata::Android(AndroidMetadata {
                        package_name: metadata.package_name,
                        version_code: metadata.version_code,
                        version_name: metadata.version_name,
                        min_sdk: metadata.min_sdk,
                        target_sdk: metadata.target_sdk,
                        certificate_sha256: metadata.certificate_sha256,
                    }),
                    icon_png: metadata.icon.map(|icon| icon.bytes),
                },
                warnings: metadata.warnings,
                _temporary: None,
            });
        }
        if extension.as_deref() == Some("aab") || extension.as_deref() == Some("apks") {
            return Err(ArtifactInputError::Invalid(
                "Android App Bundles and split APK sets are not directly installable; provide a signed standalone APK"
                    .into(),
            ));
        }
        if extension.as_deref() != Some("ipa") {
            return Err(ArtifactInputError::Invalid(
                "source file must be an IPA or APK".into(),
            ));
        }
        let (metadata, signing) = ipa::inspect_with_signing(source, requested_file_name)?;
        verify_ipa_signing(&metadata, &signing, signing_policy)?;
        return Ok(PreparedArtifact {
            path: source.to_path_buf(),
            metadata: prepared_ios_metadata(metadata),
            warnings: Vec::new(),
            _temporary: None,
        });
    }
    if !source_metadata.is_dir() || source.extension() != Some(OsStr::new("app")) {
        return Err(ArtifactInputError::Invalid(
            "source must be an IPA file or an iOS .app directory".into(),
        ));
    }

    let app = inspect_app_bundle(source)?;
    verify_code_signature(source)?;
    verify_device_architecture(&app.executable)?;
    verify_provisioning_profile(&app.provisioning_profile, &app.bundle_identifier)?;

    std::fs::create_dir_all(staging_root)?;
    let temporary = tempfile::Builder::new()
        .prefix("app-to-ipa-")
        .tempdir_in(staging_root)?;
    let file_name = requested_file_name
        .map(ipa::normalize_file_name)
        .transpose()?
        .unwrap_or_else(|| format!("{}.ipa", app.name.trim_end_matches(".app")));
    let output = temporary.path().join(&file_name);
    package_app_bundle(source, &app.name, temporary.path(), &output)?;
    // No `verify_ipa_signing` here: this IPA was just packaged from a bundle
    // whose signature, architecture, and profile were verified directly above,
    // with tools that need the bundle on disk.
    let metadata = ipa::inspect(&output, Some(&file_name))?;
    Ok(PreparedArtifact {
        path: output,
        metadata: prepared_ios_metadata(metadata),
        warnings: Vec::new(),
        _temporary: Some(temporary),
    })
}

fn prepared_ios_metadata(metadata: IpaMetadata) -> PreparedArtifactMetadata {
    PreparedArtifactMetadata {
        file_name: metadata.file_name,
        byte_count: metadata.byte_count,
        sha256: metadata.sha256,
        display_name: metadata.display_name,
        platform_metadata: PlatformMetadata::Ios(IosMetadata {
            bundle_identifier: metadata.bundle_identifier,
            bundle_version: metadata.bundle_version,
            bundle_short_version: metadata.bundle_short_version,
            minimum_os_version: metadata.minimum_os_version,
        }),
        icon_png: metadata.icon.map(|icon| icon.bytes),
    }
}

/// Reject an IPA that a device would refuse anyway.
///
/// This is deliberately weaker than the `.app` path: `codesign` and `lipo`
/// need an extracted bundle on disk, and extracting up to 2 GiB just to share
/// it is not a trade worth making. What is checkable from the archive itself —
/// that the bundle is signed at all, and that its provisioning profile is a
/// live device profile covering this bundle identifier — is checked, because
/// each of those failures otherwise surfaces on the phone as an opaque iOS
/// error after the whole download has completed.
fn verify_ipa_signing(
    metadata: &IpaMetadata,
    signing: &SigningEvidence,
    policy: SigningPolicy,
) -> Result<(), ArtifactInputError> {
    if policy == SigningPolicy::Trusted {
        tracing::warn!("--allow-unsigned: sharing without checking the IPA's signing evidence");
        return Ok(());
    }
    if !signing.has_code_signature {
        return Err(ArtifactInputError::Invalid(
            "IPA app bundle has no _CodeSignature/CodeResources, so it is not signed and iOS \
             will refuse to install it (use --allow-unsigned to share it anyway)"
                .into(),
        ));
    }
    let Some(profile) = signing.provisioning_profile.as_deref() else {
        return Err(ArtifactInputError::Invalid(
            "IPA app bundle has no embedded.mobileprovision, so it is an App Store build that \
             cannot be installed over the air (use --allow-unsigned to share it anyway)"
                .into(),
        ));
    };
    let Some(profile) = decode_provisioning_profile_bytes(profile)? else {
        return Ok(());
    };
    validate_provisioning_profile(&profile, &metadata.bundle_identifier)
}

/// Decode a CMS-wrapped provisioning profile held in memory.
///
/// Returns `Ok(None)` where there is no `security(1)` to decode it with, so a
/// non-macOS host still gets the structural checks instead of refusing every
/// IPA outright.
fn decode_provisioning_profile_bytes(bytes: &[u8]) -> Result<Option<Value>, ArtifactInputError> {
    if !cfg!(target_os = "macos") {
        tracing::warn!(
            "no macOS security(1) available; not checking the provisioning profile's \
             expiry or bundle identifier"
        );
        return Ok(None);
    }
    let temporary = tempfile::Builder::new().prefix("ipa-profile-").tempdir()?;
    let path = temporary.path().join("embedded.mobileprovision");
    std::fs::write(&path, bytes)?;
    decode_provisioning_profile(&path).map(Some)
}

fn inspect_app_bundle(path: &Path) -> Result<AppBundle, ArtifactInputError> {
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| name.ends_with(".app") && name.len() > 4)
        .ok_or_else(|| ArtifactInputError::Invalid("app bundle name is invalid".into()))?
        .to_string();
    if path.join("Contents").is_dir() {
        return Err(ArtifactInputError::Invalid(
            "macOS .app bundles cannot be installed on iOS; build for the iphoneos SDK".into(),
        ));
    }

    let info_path = path.join("Info.plist");
    require_regular_file(&info_path, "app bundle has no root Info.plist")?;
    let info = Value::from_file(&info_path)?;
    let dictionary = info
        .as_dictionary()
        .ok_or_else(|| ArtifactInputError::Invalid("app Info.plist is not a dictionary".into()))?;

    let bundle_identifier = require_nonempty_string(dictionary, "CFBundleIdentifier")?.to_string();
    if string_value(dictionary, "CFBundleVersion").is_none()
        && string_value(dictionary, "CFBundleShortVersionString").is_none()
    {
        return Err(ArtifactInputError::Invalid(
            "app Info.plist has no CFBundleVersion".into(),
        ));
    }
    if string_value(dictionary, "CFBundlePackageType") != Some("APPL") {
        return Err(ArtifactInputError::Invalid(
            "app Info.plist CFBundlePackageType must be APPL".into(),
        ));
    }
    if string_value(dictionary, "DTPlatformName") != Some("iphoneos") {
        return Err(ArtifactInputError::Invalid(
            "app is not an iphoneos device build".into(),
        ));
    }
    let supports_iphoneos = dictionary
        .get("CFBundleSupportedPlatforms")
        .and_then(Value::as_array)
        .is_some_and(|platforms| {
            platforms
                .iter()
                .any(|platform| platform.as_string() == Some("iPhoneOS"))
        });
    if !supports_iphoneos {
        return Err(ArtifactInputError::Invalid(
            "app does not declare iPhoneOS in CFBundleSupportedPlatforms".into(),
        ));
    }

    let executable_name = require_nonempty_string(dictionary, "CFBundleExecutable")?;
    if Path::new(executable_name)
        .file_name()
        .and_then(OsStr::to_str)
        != Some(executable_name)
    {
        return Err(ArtifactInputError::Invalid(
            "CFBundleExecutable must be a bundle-root file name".into(),
        ));
    }
    let executable = path.join(executable_name);
    require_regular_file(&executable, "app bundle executable is missing")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(&executable)?.permissions().mode() & 0o111 == 0 {
            return Err(ArtifactInputError::Invalid(
                "app bundle executable does not have an executable permission bit".into(),
            ));
        }
    }

    let provisioning = path.join("embedded.mobileprovision");
    require_regular_file(
        &provisioning,
        "app has no embedded.mobileprovision for development/ad hoc installation",
    )?;
    if std::fs::metadata(&provisioning)?.len() == 0 {
        return Err(ArtifactInputError::Invalid(
            "embedded.mobileprovision is empty".into(),
        ));
    }

    Ok(AppBundle {
        name,
        executable,
        bundle_identifier,
        provisioning_profile: provisioning,
    })
}

fn require_regular_file(path: &Path, message: &str) -> Result<(), ArtifactInputError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => ArtifactInputError::Invalid(message.into()),
        _ => ArtifactInputError::Io(error),
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ArtifactInputError::Invalid(message.into()));
    }
    Ok(())
}

fn require_nonempty_string<'a>(
    dictionary: &'a plist::Dictionary,
    key: &str,
) -> Result<&'a str, ArtifactInputError> {
    string_value(dictionary, key)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ArtifactInputError::Invalid(format!("app Info.plist has no {key}")))
}

fn string_value<'a>(dictionary: &'a plist::Dictionary, key: &str) -> Option<&'a str> {
    dictionary.get(key)?.as_string()
}

#[cfg(target_os = "macos")]
fn verify_code_signature(path: &Path) -> Result<(), ArtifactInputError> {
    let output = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict", "--verbose=2"])
        .arg(path)
        .output()?;
    require_command_success("codesign verification", output)
}

#[cfg(not(target_os = "macos"))]
fn verify_code_signature(_path: &Path) -> Result<(), ArtifactInputError> {
    Err(ArtifactInputError::Invalid(
        "local .app packaging requires macOS code-signing tools".into(),
    ))
}

#[cfg(target_os = "macos")]
fn verify_device_architecture(executable: &Path) -> Result<(), ArtifactInputError> {
    let output = Command::new("/usr/bin/lipo")
        .arg("-archs")
        .arg(executable)
        .output()?;
    if !output.status.success() {
        return require_command_success("lipo architecture inspection", output);
    }
    let architectures = String::from_utf8_lossy(&output.stdout);
    if architectures
        .split_whitespace()
        .any(|architecture| matches!(architecture, "arm64" | "arm64e"))
    {
        Ok(())
    } else {
        Err(ArtifactInputError::Invalid(
            "app executable has no arm64 device architecture".into(),
        ))
    }
}

#[cfg(not(target_os = "macos"))]
fn verify_device_architecture(_executable: &Path) -> Result<(), ArtifactInputError> {
    Err(ArtifactInputError::Invalid(
        "local .app packaging requires macOS architecture tools".into(),
    ))
}

fn verify_provisioning_profile(
    profile_path: &Path,
    bundle_identifier: &str,
) -> Result<(), ArtifactInputError> {
    let profile = decode_provisioning_profile(profile_path)?;
    validate_provisioning_profile(&profile, bundle_identifier)
}

#[cfg(target_os = "macos")]
fn decode_provisioning_profile(profile_path: &Path) -> Result<Value, ArtifactInputError> {
    let output = Command::new("/usr/bin/security")
        .args(["cms", "-D", "-i"])
        .arg(profile_path)
        .output()?;
    let decoded = output.stdout.clone();
    require_command_success("provisioning profile verification", output)?;
    Ok(Value::from_reader(Cursor::new(decoded))?)
}

#[cfg(not(target_os = "macos"))]
fn decode_provisioning_profile(_profile_path: &Path) -> Result<Value, ArtifactInputError> {
    Err(ArtifactInputError::Invalid(
        "local .app packaging requires macOS provisioning tools".into(),
    ))
}

fn validate_provisioning_profile(
    profile: &Value,
    bundle_identifier: &str,
) -> Result<(), ArtifactInputError> {
    let dictionary = profile.as_dictionary().ok_or_else(|| {
        ArtifactInputError::Invalid("embedded provisioning profile is not a dictionary".into())
    })?;
    let expiration = dictionary
        .get("ExpirationDate")
        .and_then(Value::as_date)
        .ok_or_else(|| {
            ArtifactInputError::Invalid(
                "embedded provisioning profile has no ExpirationDate".into(),
            )
        })?;
    if SystemTime::from(expiration) <= SystemTime::now() {
        return Err(ArtifactInputError::Invalid(
            "embedded provisioning profile has expired".into(),
        ));
    }

    let supports_device_install = dictionary
        .get("ProvisionedDevices")
        .and_then(Value::as_array)
        .is_some_and(|devices| !devices.is_empty())
        || dictionary
            .get("ProvisionsAllDevices")
            .and_then(Value::as_boolean)
            == Some(true);
    if !supports_device_install {
        return Err(ArtifactInputError::Invalid(
            "embedded profile is not a development, ad hoc, or enterprise device profile".into(),
        ));
    }

    let application_identifier = dictionary
        .get("Entitlements")
        .and_then(Value::as_dictionary)
        .and_then(|entitlements| entitlements.get("application-identifier"))
        .and_then(Value::as_string)
        .ok_or_else(|| {
            ArtifactInputError::Invalid(
                "embedded provisioning profile has no application-identifier".into(),
            )
        })?;
    let provisioned_bundle_identifier = application_identifier
        .split_once('.')
        .map(|(_, identifier)| identifier)
        .ok_or_else(|| {
            ArtifactInputError::Invalid(
                "embedded provisioning profile application-identifier is invalid".into(),
            )
        })?;
    let matches = provisioned_bundle_identifier == bundle_identifier
        || provisioned_bundle_identifier
            .strip_suffix('*')
            .is_some_and(|prefix| bundle_identifier.starts_with(prefix));
    if !matches {
        return Err(ArtifactInputError::Invalid(format!(
            "embedded provisioning profile does not allow bundle identifier {bundle_identifier}"
        )));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn package_app_bundle(
    source: &Path,
    app_name: &str,
    temporary_root: &Path,
    output: &Path,
) -> Result<(), ArtifactInputError> {
    let payload = temporary_root.join("Payload");
    std::fs::create_dir(&payload)?;
    let copy = Command::new("/usr/bin/ditto")
        .args(["--norsrc", "--noextattr"])
        .arg(source)
        .arg(payload.join(app_name))
        .output()?;
    require_command_success("ditto app copy", copy)?;
    let archive = Command::new("/usr/bin/ditto")
        .args(["-c", "-k", "--norsrc", "--noextattr", "--keepParent"])
        .arg(&payload)
        .arg(output)
        .output()?;
    require_command_success("ditto IPA packaging", archive)
}

#[cfg(not(target_os = "macos"))]
fn package_app_bundle(
    _source: &Path,
    _app_name: &str,
    _temporary_root: &Path,
    _output: &Path,
) -> Result<(), ArtifactInputError> {
    Err(ArtifactInputError::Invalid(
        "local .app packaging requires macOS ditto".into(),
    ))
}

fn require_command_success(tool: &'static str, output: Output) -> Result<(), ArtifactInputError> {
    if output.status.success() {
        return Ok(());
    }
    let bytes = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    let message = String::from_utf8_lossy(bytes)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Err(ArtifactInputError::Tool {
        tool,
        message: if message.is_empty() {
            "no diagnostic output".into()
        } else {
            message.chars().take(2_000).collect()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provisioning_profile(
        application_identifier: &str,
        expiration: SystemTime,
        includes_devices: bool,
    ) -> Value {
        let mut dictionary = plist::Dictionary::new();
        dictionary.insert(
            "ExpirationDate".into(),
            Value::Date(plist::Date::from(expiration)),
        );
        let mut entitlements = plist::Dictionary::new();
        entitlements.insert(
            "application-identifier".to_string(),
            Value::String(application_identifier.into()),
        );
        dictionary.insert("Entitlements".into(), Value::Dictionary(entitlements));
        if includes_devices {
            dictionary.insert(
                "ProvisionedDevices".into(),
                Value::Array(vec![Value::String("device-udid".into())]),
            );
        }
        Value::Dictionary(dictionary)
    }

    fn write_app(root: &Path, platform: &str, supported_platform: &str) -> PathBuf {
        let app = root.join("Example.app");
        std::fs::create_dir(&app).unwrap();
        let dictionary = [
            (
                "CFBundleIdentifier",
                Value::String("com.example.app".into()),
            ),
            ("CFBundleVersion", Value::String("1".into())),
            ("CFBundlePackageType", Value::String("APPL".into())),
            ("CFBundleExecutable", Value::String("Example".into())),
            ("DTPlatformName", Value::String(platform.into())),
            (
                "CFBundleSupportedPlatforms",
                Value::Array(vec![Value::String(supported_platform.into())]),
            ),
            (
                "CFBundleIconFiles",
                Value::Array(vec![Value::String("AppIcon60x60".into())]),
            ),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect();
        Value::Dictionary(dictionary)
            .to_file_xml(app.join("Info.plist"))
            .unwrap();
        std::fs::write(app.join("Example"), b"not a real Mach-O").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(app.join("Example"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        std::fs::write(
            app.join("AppIcon60x60@3x.png"),
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\xb4\x00\x00\x00\xb4\x08\x06\x00\x00\x00\x00\x00\x00\x00",
        )
        .unwrap();
        std::fs::write(app.join("embedded.mobileprovision"), b"profile").unwrap();
        app
    }

    #[test]
    fn accepts_structurally_complete_iphoneos_app() {
        let temporary = tempfile::tempdir().unwrap();
        let app = write_app(temporary.path(), "iphoneos", "iPhoneOS");
        let inspected = inspect_app_bundle(&app).unwrap();
        assert_eq!(inspected.name, "Example.app");
        assert_eq!(inspected.executable, app.join("Example"));
    }

    #[test]
    fn rejects_simulator_app() {
        let temporary = tempfile::tempdir().unwrap();
        let app = write_app(temporary.path(), "iphonesimulator", "iPhoneSimulator");
        let error = inspect_app_bundle(&app).unwrap_err().to_string();
        assert!(error.contains("not an iphoneos device build"));
    }

    #[test]
    fn rejects_macos_bundle_shape() {
        let temporary = tempfile::tempdir().unwrap();
        let app = temporary.path().join("Example.app");
        std::fs::create_dir_all(app.join("Contents")).unwrap();
        let error = inspect_app_bundle(&app).unwrap_err().to_string();
        assert!(error.contains("macOS .app"));
    }

    #[test]
    fn rejects_app_without_provisioning_profile() {
        let temporary = tempfile::tempdir().unwrap();
        let app = write_app(temporary.path(), "iphoneos", "iPhoneOS");
        std::fs::remove_file(app.join("embedded.mobileprovision")).unwrap();
        let error = inspect_app_bundle(&app).unwrap_err().to_string();
        assert!(error.contains("embedded.mobileprovision"));
    }

    #[test]
    fn accepts_matching_device_provisioning_profile() {
        let profile = provisioning_profile(
            "TEAMID.com.example.*",
            SystemTime::now() + std::time::Duration::from_secs(60),
            true,
        );
        validate_provisioning_profile(&profile, "com.example.app").unwrap();
    }

    #[test]
    fn rejects_app_store_and_mismatched_profiles() {
        let future = SystemTime::now() + std::time::Duration::from_secs(60);
        let app_store = provisioning_profile("TEAMID.com.example.app", future, false);
        assert!(
            validate_provisioning_profile(&app_store, "com.example.app")
                .unwrap_err()
                .to_string()
                .contains("not a development")
        );

        let mismatched = provisioning_profile("TEAMID.com.other.*", future, true);
        assert!(
            validate_provisioning_profile(&mismatched, "com.example.app")
                .unwrap_err()
                .to_string()
                .contains("does not allow bundle identifier")
        );
    }

    #[test]
    fn rejects_expired_provisioning_profile() {
        let profile = provisioning_profile(
            "TEAMID.com.example.app",
            SystemTime::now() - std::time::Duration::from_secs(60),
            true,
        );
        assert!(
            validate_provisioning_profile(&profile, "com.example.app")
                .unwrap_err()
                .to_string()
                .contains("expired")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn packages_app_as_a_canonical_ipa() {
        let temporary = tempfile::tempdir().unwrap();
        let app = write_app(temporary.path(), "iphoneos", "iPhoneOS");
        let packaging = tempfile::tempdir().unwrap();
        let output = packaging.path().join("Example.ipa");
        package_app_bundle(&app, "Example.app", packaging.path(), &output).unwrap();
        let metadata = ipa::inspect(&output, None).unwrap();
        assert_eq!(metadata.bundle_identifier, "com.example.app");
        assert_eq!(metadata.bundle_version, "1");
        let icon = metadata.icon.expect("app icon");
        assert_eq!((icon.width, icon.height), (180, 180));
    }

    fn ipa_metadata(bundle_identifier: &str) -> IpaMetadata {
        IpaMetadata {
            file_name: "Example.ipa".into(),
            byte_count: 1,
            sha256: "abc".into(),
            bundle_identifier: bundle_identifier.into(),
            bundle_version: "1".into(),
            bundle_short_version: None,
            display_name: None,
            minimum_os_version: None,
            icon: None,
        }
    }

    /// The failures worth catching locally are exactly the ones that would
    /// otherwise cost the recipient a full download before iOS refuses.
    #[test]
    fn an_unsigned_or_app_store_ipa_is_rejected_before_sharing() {
        let metadata = ipa_metadata("com.example.app");

        let unsigned = SigningEvidence::default();
        let error = verify_ipa_signing(&metadata, &unsigned, SigningPolicy::Required)
            .unwrap_err()
            .to_string();
        assert!(error.contains("_CodeSignature"), "{error}");
        assert!(error.contains("--allow-unsigned"), "{error}");

        let app_store = SigningEvidence {
            has_code_signature: true,
            provisioning_profile: None,
        };
        let error = verify_ipa_signing(&metadata, &app_store, SigningPolicy::Required)
            .unwrap_err()
            .to_string();
        assert!(error.contains("embedded.mobileprovision"), "{error}");
        assert!(error.contains("App Store"), "{error}");
    }

    /// `--allow-unsigned` has to keep working on exactly the inputs the
    /// checks reject, or it is not an escape hatch.
    #[test]
    fn allow_unsigned_shares_an_ipa_the_checks_would_refuse() {
        let metadata = ipa_metadata("com.example.app");
        for evidence in [
            SigningEvidence::default(),
            SigningEvidence {
                has_code_signature: true,
                provisioning_profile: None,
            },
            SigningEvidence {
                has_code_signature: true,
                provisioning_profile: Some(b"not a real CMS blob".to_vec()),
            },
        ] {
            verify_ipa_signing(&metadata, &evidence, SigningPolicy::Trusted).unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_undecodable_provisioning_profile_is_reported_rather_than_ignored() {
        let metadata = ipa_metadata("com.example.app");
        let garbage = SigningEvidence {
            has_code_signature: true,
            provisioning_profile: Some(b"definitely not CMS".to_vec()),
        };
        assert!(verify_ipa_signing(&metadata, &garbage, SigningPolicy::Required).is_err());
    }
}
