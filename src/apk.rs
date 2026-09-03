//! Inspection of a signed, monolithic Android APK.
//!
//! APKs are ZIP files, but the ZIP layout alone is not enough to establish
//! that a file is installable.  The Android SDK tools own the two pieces of
//! information that are easy to get wrong here: decoded manifest metadata
//! and APK signature verification.  This module therefore does only the
//! cheap structural checks itself and delegates the Android-specific parsing
//! and verification to `apkanalyzer` and `apksigner` when they are available.
//! Missing automatically discovered tools produce explicit inspection warnings;
//! an explicitly configured tool or a discovered tool that fails remains a
//! hard error.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use thiserror::Error;
use zip::ZipArchive;

use crate::model::MAX_ARTIFACT_BYTES;

const MAX_MANIFEST_ENTRY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_MANIFEST_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOOL_DIAGNOSTIC_BYTES: usize = 4 * 1024;

/// Android SDK command-line tools needed to inspect an APK.
///
/// The paths are kept together so discovery and invocation do not leak into
/// the CLI or artifact-input layers.  Callers that need deterministic tool
/// selection can use [`ApkToolchain::new`] with explicit paths; callers that
/// want the normal SDK lookup order can use [`ApkToolchain::discover`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApkToolchain {
    /// Path to the official Android SDK `apkanalyzer` executable/script.
    pub apkanalyzer: Option<PathBuf>,
    /// Path to the official Android SDK Build Tools `apksigner` executable.
    pub apksigner: Option<PathBuf>,
}

impl ApkToolchain {
    /// Construct a toolchain from explicitly resolved tool paths.
    pub fn new(apkanalyzer: impl Into<PathBuf>, apksigner: impl Into<PathBuf>) -> Self {
        Self {
            apkanalyzer: Some(apkanalyzer.into()),
            apksigner: Some(apksigner.into()),
        }
    }

    /// Construct a toolchain with independently optional tools.
    pub fn from_optional_paths(apkanalyzer: Option<PathBuf>, apksigner: Option<PathBuf>) -> Self {
        Self {
            apkanalyzer,
            apksigner,
        }
    }

    /// Alias useful at call sites that build a toolchain from two path values.
    pub fn from_paths(apkanalyzer: impl Into<PathBuf>, apksigner: impl Into<PathBuf>) -> Self {
        Self::new(apkanalyzer, apksigner)
    }

    /// Discover available Android SDK tools in one place.
    ///
    /// Each optional override is authoritative when supplied.  An invalid
    /// override returns an error instead of silently falling through to a
    /// different executable.  When an override is absent, lookup proceeds in
    /// this order for each tool:
    ///
    /// 1. the process `PATH`;
    /// 2. `ANDROID_SDK_ROOT`, then `ANDROID_HOME`;
    /// 3. standard per-user SDK locations for the host platform.
    ///
    /// A tool that is not explicitly requested and cannot be found remains
    /// unavailable so inspection can report a non-fatal warning. `apkanalyzer`
    /// prefers `cmdline-tools/latest/bin`, then the highest
    /// numbered `cmdline-tools/<version>/bin` directory.  `apksigner` uses the
    /// highest numbered `build-tools/<version>` directory.  Every candidate
    /// must be an executable regular file.
    pub fn discover(
        apkanalyzer_override: Option<&Path>,
        apksigner_override: Option<&Path>,
    ) -> Result<Self, ApkError> {
        let sdk_roots = sdk_roots();
        let apkanalyzer = resolve_optional_tool(
            "apkanalyzer",
            "--apkanalyzer-bin",
            apkanalyzer_override,
            path_candidates("apkanalyzer", &sdk_roots),
        )?;
        let apksigner = resolve_optional_tool(
            "apksigner",
            "--apksigner-bin",
            apksigner_override,
            path_candidates("apksigner", &sdk_roots),
        )?;
        Ok(Self::from_optional_paths(apkanalyzer, apksigner))
    }
}

/// A future-proof place for an extracted APK icon.  Icon extraction is not
/// needed by the first Android flow, so `inspect` currently always returns
/// `None` for [`ApkMetadata::icon`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApkIcon {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Metadata needed by the platform-neutral sharing and install layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApkMetadata {
    pub file_name: String,
    pub byte_count: u64,
    pub sha256: String,
    pub package_name: Option<String>,
    /// Kept as text so callers can preserve the exact Android manifest value
    /// without imposing a version-code policy on the inspector.
    pub version_code: Option<String>,
    pub version_name: Option<String>,
    pub min_sdk: Option<u32>,
    pub target_sdk: Option<u32>,
    pub display_name: Option<String>,
    pub icon: Option<ApkIcon>,
    /// The SHA-256 digest printed by `apksigner` for the APK signer.
    pub certificate_sha256: Option<String>,
    /// Checks that could not run because an optional SDK tool was unavailable.
    pub warnings: Vec<String>,
}

/// Errors returned while checking an APK or invoking the configured Android
/// SDK tools.
#[derive(Debug, Error)]
pub enum ApkError {
    #[error("APK file I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("APK archive is invalid: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("APK is invalid: {0}")]
    Invalid(String),
    #[error("APK split packages are not supported: {0}")]
    UnsupportedSplit(String),
    #[error("Android SDK tool {tool} at {path} could not be started: {source}")]
    ToolIo {
        tool: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Android SDK tool {tool} at {path} failed ({status}): {diagnostic}")]
    ToolFailed {
        tool: &'static str,
        path: PathBuf,
        status: String,
        diagnostic: String,
    },
    #[error("Android SDK tool {tool} at {path} emitted invalid UTF-8")]
    ToolOutputUtf8 {
        tool: &'static str,
        path: PathBuf,
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("Android SDK tool {tool} at {path} emitted more than {limit} bytes")]
    ToolOutputTooLarge {
        tool: &'static str,
        path: PathBuf,
        limit: usize,
    },
}

/// Inspect a signed, monolithic APK using the explicitly supplied Android SDK
/// tools.
///
/// `requested_file_name` is the name that the sharing layer intends to expose
/// to downloaders.  It is normalized to a basename and must end in `.apk`.
/// When it is absent, the source file's basename is used.  The source itself
/// must also have an `.apk` extension; an AAB or a renamed archive is not
/// accepted as an APK.
pub fn inspect(
    path: &Path,
    requested_file_name: Option<&str>,
    toolchain: &ApkToolchain,
) -> Result<ApkMetadata, ApkError> {
    let source_metadata = std::fs::symlink_metadata(path)?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_file() {
        return Err(ApkError::Invalid(
            "APK source must be a regular file, not a directory or symbolic link".into(),
        ));
    }
    if source_metadata.len() == 0 || source_metadata.len() > MAX_ARTIFACT_BYTES {
        return Err(ApkError::Invalid(format!(
            "APK must be between 1 byte and {MAX_ARTIFACT_BYTES} bytes (2 GiB)"
        )));
    }

    let source_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| ApkError::Invalid("APK source file name is missing or not UTF-8".into()))?;
    if !has_apk_extension(source_name) {
        return Err(ApkError::Invalid(format!(
            "APK source must have a .apk extension (got {source_name:?}); .aab files and split bundles are not directly installable"
        )));
    }
    let file_name = normalize_file_name(requested_file_name.unwrap_or(source_name))?;

    let manifest_index = validate_zip(path)?;

    // Reading the manifest entry proves that the ZIP member is not merely in
    // the central directory with unreadable/encrypted content.  The Android
    // tool still owns the actual binary-XML decoding below.
    read_manifest_entry(path, manifest_index)?;

    let mut warnings = Vec::new();
    let (package_name, version_code, version_name, min_sdk, target_sdk, display_name) =
        if let Some(analyzer) = toolchain.apkanalyzer.as_deref() {
            let manifest_print = run_tool(analyzer, "apkanalyzer", &["manifest", "print"], path)?;
            let printed = parse_manifest_print(&manifest_print)?;
            if let Some(reason) = printed.split_reason {
                return Err(ApkError::UnsupportedSplit(reason));
            }

            (
                Some(required_tool_value(
                    "application ID",
                    run_tool(
                        analyzer,
                        "apkanalyzer",
                        &["manifest", "application-id"],
                        path,
                    )?,
                )?),
                Some(required_tool_value(
                    "version code",
                    run_tool(analyzer, "apkanalyzer", &["manifest", "version-code"], path)?,
                )?),
                optional_tool_value(run_tool(
                    analyzer,
                    "apkanalyzer",
                    &["manifest", "version-name"],
                    path,
                )?)?,
                parse_sdk_value(
                    "minSdkVersion",
                    run_tool(analyzer, "apkanalyzer", &["manifest", "min-sdk"], path)?,
                )?,
                parse_sdk_value(
                    "targetSdkVersion",
                    run_tool(analyzer, "apkanalyzer", &["manifest", "target-sdk"], path)?,
                )?,
                printed.display_name,
            )
        } else {
            warnings.push(
                "apkanalyzer was not found; continuing without Android manifest metadata or split-APK validation. Install Android SDK Command-line Tools or pass --apkanalyzer-bin <PATH>."
                    .into(),
            );
            (None, None, None, None, None, None)
        };

    let certificate_sha256 = if let Some(signer) = toolchain.apksigner.as_deref() {
        Some(extract_certificate_sha256(&run_tool(
            signer,
            "apksigner",
            &["verify", "--verbose", "--print-certs"],
            path,
        )?)?)
    } else {
        warnings.push(
            "apksigner was not found; continuing without APK signature verification. Android may reject an unsigned or incorrectly signed APK. Install Android SDK Build Tools or pass --apksigner-bin <PATH>."
                .into(),
        );
        None
    };

    Ok(ApkMetadata {
        file_name,
        byte_count: source_metadata.len(),
        sha256: sha256_file(path)?,
        package_name,
        version_code,
        version_name,
        min_sdk,
        target_sdk,
        display_name,
        icon: None,
        certificate_sha256,
        warnings,
    })
}

/// Normalize a user-facing APK file name to a basename and require the
/// installable `.apk` suffix.
pub fn normalize_file_name(value: &str) -> Result<String, ApkError> {
    let trimmed = value.trim();
    let file_name = Path::new(trimmed)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApkError::Invalid("APK file name is invalid".into()))?;
    if !has_apk_extension(file_name) {
        return Err(ApkError::Invalid(format!(
            "APK file name must end with .apk (got {file_name:?})"
        )));
    }
    Ok(file_name.to_string())
}

fn has_apk_extension(value: &str) -> bool {
    Path::new(value)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("apk"))
}

fn resolve_optional_tool(
    tool: &'static str,
    override_flag: &'static str,
    override_path: Option<&Path>,
    sdk_candidates: Vec<PathBuf>,
) -> Result<Option<PathBuf>, ApkError> {
    if let Some(path) = override_path {
        if is_executable_file(path) {
            return Ok(Some(path.to_path_buf()));
        }
        return Err(ApkError::Invalid(format!(
            "{override_flag} points to {path:?}, but it is not an executable regular file; provide the path to the Android SDK {tool} binary"
        )));
    }

    if let Some(path) = path_lookup(tool) {
        return Ok(Some(path));
    }
    if let Some(path) = sdk_candidates
        .into_iter()
        .find(|path| is_executable_file(path))
    {
        return Ok(Some(path));
    }

    Ok(None)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn path_lookup(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        for candidate_name in executable_names(tool) {
            let candidate = directory.join(candidate_name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn executable_names(tool: &str) -> Vec<&str> {
    vec![tool]
}

#[cfg(windows)]
fn executable_names(tool: &str) -> Vec<String> {
    vec![
        tool.to_string(),
        format!("{tool}.exe"),
        format!("{tool}.cmd"),
        format!("{tool}.bat"),
    ]
}

fn sdk_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for variable in ["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Some(value) = std::env::var_os(variable) {
            let path = PathBuf::from(value);
            if !path.as_os_str().is_empty() && !roots.iter().any(|root| root == &path) {
                roots.push(path);
            }
        }
    }

    if let Some(home) = home_directory() {
        for path in standard_sdk_roots(&home) {
            if !roots.iter().any(|root| root == &path) {
                roots.push(path);
            }
        }
    }
    roots
}

#[cfg(target_os = "macos")]
fn standard_sdk_roots(home: &Path) -> Vec<PathBuf> {
    vec![home.join("Library/Android/sdk"), home.join("Android/Sdk")]
}

#[cfg(target_os = "linux")]
fn standard_sdk_roots(home: &Path) -> Vec<PathBuf> {
    vec![home.join("Android/Sdk"), home.join("Library/Android/sdk")]
}

#[cfg(windows)]
fn standard_sdk_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots = vec![home.join("AppData/Local/Android/Sdk")];
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        roots.push(PathBuf::from(local_app_data).join("Android/Sdk"));
    }
    roots
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn standard_sdk_roots(_home: &Path) -> Vec<PathBuf> {
    Vec::new()
}

fn home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

fn path_candidates(tool: &str, roots: &[PathBuf]) -> Vec<PathBuf> {
    match tool {
        "apkanalyzer" => cmdline_tool_candidates(tool, roots),
        "apksigner" => build_tool_candidates(tool, roots),
        _ => Vec::new(),
    }
}

fn cmdline_tool_candidates(tool: &str, roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let names = executable_names(tool);

    // `latest` is an explicit SDK convention and wins over numbered
    // cmdline-tools directories regardless of the version those directories
    // happen to contain.
    for root in roots {
        for name in &names {
            candidates.push(root.join("cmdline-tools/latest/bin").join(name));
        }
    }

    let mut versions = sdk_version_directories(roots, "cmdline-tools");
    versions.sort_by(version_directory_order);
    for (_, directory) in versions {
        for name in &names {
            candidates.push(directory.join("bin").join(name));
        }
    }

    // Older SDKs shipped `apkanalyzer` under `tools/bin`; retain that path as
    // a final SDK-local option without allowing it to outrank cmdline-tools.
    for root in roots {
        for name in &names {
            candidates.push(root.join("tools/bin").join(name));
        }
    }
    candidates
}

fn build_tool_candidates(tool: &str, roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let names = executable_names(tool);
    let mut versions = sdk_version_directories(roots, "build-tools");
    versions.sort_by(version_directory_order);
    for (_, directory) in versions {
        for name in &names {
            candidates.push(directory.join(name));
        }
    }
    candidates
}

fn sdk_version_directories(roots: &[PathBuf], component: &str) -> Vec<(usize, PathBuf)> {
    let mut directories = Vec::new();
    for (root_index, root) in roots.iter().enumerate() {
        let Ok(entries) = std::fs::read_dir(root.join(component)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            if name.eq_ignore_ascii_case("latest") {
                continue;
            }
            directories.push((root_index, path));
        }
    }
    directories
}

fn version_directory_order(
    left: &(usize, PathBuf),
    right: &(usize, PathBuf),
) -> std::cmp::Ordering {
    let left_name = left
        .1
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    let right_name = right
        .1
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    sdk_version_key(right_name)
        .cmp(&sdk_version_key(left_name))
        // If a stable directory and a suffixed preview directory have the
        // same numeric prefix, prefer the exact numeric spelling.
        .then_with(|| {
            is_exact_numeric_version(right_name).cmp(&is_exact_numeric_version(left_name))
        })
        // Preserve environment-variable root order for otherwise equal SDKs.
        .then_with(|| left.0.cmp(&right.0))
        .then_with(|| right_name.cmp(left_name))
}

fn sdk_version_key(value: &str) -> Vec<u64> {
    value
        .split(|character: char| !character.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u64>().ok())
        .collect()
}

fn is_exact_numeric_version(value: &str) -> bool {
    !value.is_empty()
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn validate_zip(path: &Path) -> Result<usize, ApkError> {
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut manifest_index = None;

    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name();
        if !safe_zip_name(name) {
            return Err(ApkError::Invalid(format!(
                "APK contains an unsafe ZIP entry name: {name:?}"
            )));
        }
        if is_symlink_entry(entry.unix_mode()) {
            return Err(ApkError::Invalid(format!(
                "APK contains a symbolic-link ZIP entry: {name:?}"
            )));
        }
        if name == "AndroidManifest.xml" {
            if entry.is_dir() {
                return Err(ApkError::Invalid(
                    "APK AndroidManifest.xml entry is a directory".into(),
                ));
            }
            if manifest_index.replace(index).is_some() {
                return Err(ApkError::Invalid(
                    "APK contains duplicate AndroidManifest.xml entries".into(),
                ));
            }
        }
    }

    manifest_index.ok_or_else(|| {
        ApkError::Invalid(
            "APK does not contain a root AndroidManifest.xml; provide a standalone APK rather than an AAB or split archive".into(),
        )
    })
}

fn read_manifest_entry(path: &Path, index: usize) -> Result<(), ApkError> {
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut entry = archive.by_index(index)?;
    if entry.encrypted() {
        return Err(ApkError::Invalid(
            "APK AndroidManifest.xml is encrypted and cannot be inspected".into(),
        ));
    }
    if entry.size() == 0 || entry.size() > MAX_MANIFEST_ENTRY_BYTES {
        return Err(ApkError::Invalid(format!(
            "APK AndroidManifest.xml must be between 1 byte and {MAX_MANIFEST_ENTRY_BYTES} bytes"
        )));
    }
    let mut bytes = Vec::with_capacity(entry.size().min(MAX_MANIFEST_ENTRY_BYTES) as usize);
    entry
        .by_ref()
        .take(MAX_MANIFEST_ENTRY_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST_ENTRY_BYTES {
        return Err(ApkError::Invalid(format!(
            "APK AndroidManifest.xml exceeds the {MAX_MANIFEST_ENTRY_BYTES}-byte inspection limit"
        )));
    }
    Ok(())
}

fn safe_zip_name(name: &str) -> bool {
    let normalized = name.strip_suffix('/').unwrap_or(name);
    !normalized.starts_with('/')
        && !normalized
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn is_symlink_entry(unix_mode: Option<u32>) -> bool {
    const S_IFMT: u32 = 0o170000;
    const S_IFLNK: u32 = 0o120000;
    unix_mode.is_some_and(|mode| mode & S_IFMT == S_IFLNK)
}

fn sha256_file(path: &Path) -> Result<String, ApkError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn run_tool(
    path: &Path,
    tool: &'static str,
    arguments: &[&str],
    apk_path: &Path,
) -> Result<String, ApkError> {
    let output = Command::new(path)
        .args(arguments)
        .arg(apk_path)
        .output()
        .map_err(|source| ApkError::ToolIo {
            tool,
            path: path.to_path_buf(),
            source,
        })?;

    if !output.status.success() {
        return Err(ApkError::ToolFailed {
            tool,
            path: path.to_path_buf(),
            status: output
                .status
                .code()
                .map(|code| format!("exit code {code}"))
                .unwrap_or_else(|| "terminated by signal".into()),
            diagnostic: tool_diagnostic(&output.stderr, &output.stdout),
        });
    }
    if output.stdout.len() > MAX_MANIFEST_OUTPUT_BYTES {
        return Err(ApkError::ToolOutputTooLarge {
            tool,
            path: path.to_path_buf(),
            limit: MAX_MANIFEST_OUTPUT_BYTES,
        });
    }
    String::from_utf8(output.stdout).map_err(|source| ApkError::ToolOutputUtf8 {
        tool,
        path: path.to_path_buf(),
        source,
    })
}

fn tool_diagnostic(stderr: &[u8], stdout: &[u8]) -> String {
    let source = if !stderr.is_empty() { stderr } else { stdout };
    let text = String::from_utf8_lossy(source);
    let text = text.trim();
    if text.is_empty() {
        return "no diagnostic output".into();
    }
    text.chars().take(MAX_TOOL_DIAGNOSTIC_BYTES).collect()
}

fn required_tool_value(field: &str, output: String) -> Result<String, ApkError> {
    let value = output.trim();
    if value.is_empty() || value.contains(['\r', '\n']) || is_unknown_value(value) {
        return Err(ApkError::Invalid(format!(
            "apkanalyzer did not return a usable {field}; verify the APK manifest and SDK tool version"
        )));
    }
    Ok(value.to_string())
}

fn optional_tool_value(output: String) -> Result<Option<String>, ApkError> {
    let value = output.trim();
    if value.is_empty() || is_unknown_value(value) {
        return Ok(None);
    }
    if value.contains(['\r', '\n']) {
        return Err(ApkError::Invalid(
            "apkanalyzer returned a multi-line version name".into(),
        ));
    }
    Ok(Some(value.to_string()))
}

fn parse_sdk_value(field: &str, output: String) -> Result<Option<u32>, ApkError> {
    let Some(value) = optional_tool_value(output)? else {
        return Ok(None);
    };
    value.parse::<u32>().map(Some).map_err(|_| {
        ApkError::Invalid(format!(
            "apkanalyzer returned an invalid {field} value {value:?}; expected an integer SDK level"
        ))
    })
}

fn is_unknown_value(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "?" | "unknown" | "null" | "none"
    )
}

#[derive(Debug, Default)]
struct PrintedManifest {
    split_reason: Option<String>,
    display_name: Option<String>,
}

fn parse_manifest_print(output: &str) -> Result<PrintedManifest, ApkError> {
    let Some((manifest_start, manifest_end)) = find_tag(output, "manifest", 0) else {
        return Err(ApkError::Invalid(
            "apkanalyzer manifest print did not return a <manifest> element".into(),
        ));
    };
    let manifest_attrs =
        parse_attributes(&output[manifest_start + "<manifest".len()..manifest_end])?;

    let mut split_reason = manifest_attrs.iter().find_map(|(name, value)| {
        if matches!(name.as_str(), "split" | "android:split") {
            Some(format!("manifest declares split={value:?}"))
        } else if matches!(name.as_str(), "isFeatureSplit" | "android:isFeatureSplit")
            && matches!(value.to_ascii_lowercase().as_str(), "true" | "1")
        {
            Some("manifest declares isFeatureSplit=true".into())
        } else {
            None
        }
    });

    let application_attributes = find_tag(output, "application", manifest_end)
        .map(|(application_start, application_end)| {
            parse_attributes(&output[application_start + "<application".len()..application_end])
        })
        .transpose()?
        .unwrap_or_default();
    if split_reason.is_none()
        && application_attributes.iter().any(|(name, value)| {
            matches!(name.as_str(), "android:isSplitRequired" | "isSplitRequired")
                && matches!(value.to_ascii_lowercase().as_str(), "true" | "1")
        })
    {
        split_reason = Some("application declares isSplitRequired=true".into());
    }
    let display_name = application_attributes.iter().find_map(|(name, value)| {
        if matches!(name.as_str(), "android:label" | "label") {
            literal_display_name(value)
        } else {
            None
        }
    });

    Ok(PrintedManifest {
        split_reason,
        display_name,
    })
}

/// Find the bounds of a start tag, including `<` and `>`.
fn find_tag(output: &str, tag_name: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = output.as_bytes();
    let mut cursor = from.min(bytes.len());
    while cursor < bytes.len() {
        let relative = output[cursor..].find('<')?;
        let start = cursor + relative;
        if output[start..].starts_with("<!--") {
            cursor = output[start + 4..].find("-->")? + start + 7;
            continue;
        }
        if output[start..].starts_with("<?") {
            cursor = output[start + 2..].find("?>")? + start + 4;
            continue;
        }
        if output[start..].starts_with("<!") {
            cursor = output[start + 2..].find('>')? + start + 3;
            continue;
        }

        let name_start = start + 1;
        let mut name_end = name_start;
        while name_end < bytes.len()
            && !bytes[name_end].is_ascii_whitespace()
            && !matches!(bytes[name_end], b'>' | b'/')
        {
            name_end += 1;
        }
        if &output[name_start..name_end] == tag_name {
            let boundary = name_end == bytes.len()
                || bytes[name_end].is_ascii_whitespace()
                || matches!(bytes[name_end], b'>' | b'/');
            if boundary {
                let end = find_tag_end(output, start + 1)?;
                return Some((start, end));
            }
        }
        cursor = start + 1;
    }
    None
}

fn find_tag_end(output: &str, from: usize) -> Option<usize> {
    let bytes = output.as_bytes();
    let mut quote = None;
    for (offset, byte) in bytes[from..].iter().copied().enumerate() {
        match quote {
            Some(expected) if byte == expected => quote = None,
            Some(_) => {}
            None if byte == b'"' || byte == b'\'' => quote = Some(byte),
            None if byte == b'>' => return Some(from + offset),
            None => {}
        }
    }
    None
}

fn parse_attributes(source: &str) -> Result<Vec<(String, String)>, ApkError> {
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut attributes = Vec::new();
    while index < bytes.len() {
        while index < bytes.len() && (bytes[index].is_ascii_whitespace() || bytes[index] == b'/') {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let name_start = index;
        while index < bytes.len()
            && !bytes[index].is_ascii_whitespace()
            && !matches!(bytes[index], b'=' | b'/' | b'>')
        {
            index += 1;
        }
        if name_start == index {
            return Err(ApkError::Invalid(
                "apkanalyzer manifest print contained a malformed attribute name".into(),
            ));
        }
        let name = &source[name_start..index];
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes.get(index) != Some(&b'=') {
            return Err(ApkError::Invalid(format!(
                "apkanalyzer manifest print attribute {name:?} has no value"
            )));
        }
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let Some(&quote) = bytes.get(index) else {
            return Err(ApkError::Invalid(format!(
                "apkanalyzer manifest print attribute {name:?} has no quoted value"
            )));
        };
        if quote != b'"' && quote != b'\'' {
            return Err(ApkError::Invalid(format!(
                "apkanalyzer manifest print attribute {name:?} has an unquoted value"
            )));
        }
        index += 1;
        let value_start = index;
        while index < bytes.len() && bytes[index] != quote {
            index += 1;
        }
        if index >= bytes.len() {
            return Err(ApkError::Invalid(format!(
                "apkanalyzer manifest print attribute {name:?} has an unterminated value"
            )));
        }
        let value = decode_xml_entities(&source[value_start..index]);
        index += 1;
        attributes.push((name.to_string(), value));
    }
    Ok(attributes)
}

fn decode_xml_entities(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn literal_display_name(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('@') || value.starts_with('?') {
        return None;
    }
    Some(value.to_string())
}

fn extract_certificate_sha256(output: &str) -> Result<String, ApkError> {
    const MARKER: &str = "certificate sha-256 digest:";
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        let Some(index) = lower.find(MARKER) else {
            continue;
        };
        let value = line[index + MARKER.len()..]
            .split_whitespace()
            .next()
            .unwrap_or_default();
        if let Some(value) = valid_sha256_digest(value) {
            return Ok(value);
        }
    }
    Err(ApkError::Invalid(
        "apksigner verified no SHA-256 certificate digest; ensure the APK is signed and the configured Build Tools are current".into(),
    ))
}

fn valid_sha256_digest(value: &str) -> Option<String> {
    let compact: String = value
        .chars()
        .filter(|character| *character != ':')
        .collect();
    if compact.len() != 64 || !compact.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_structural_apk(path: &Path) {
        let file = File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file::<_, ()>("AndroidManifest.xml", zip::write::FileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut writer, b"binary manifest fixture").unwrap();
        writer.finish().unwrap();
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, contents).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn normalizes_apk_names_and_rejects_other_formats() {
        assert_eq!(normalize_file_name("/tmp/Build.APK").unwrap(), "Build.APK");
        assert!(normalize_file_name("Build.aab").is_err());
        assert!(normalize_file_name("Build").is_err());
    }

    #[test]
    fn parses_manifest_split_markers_and_literal_label() {
        let output = r#"<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android" package="com.example" split="config.en">
  <application android:label="Example &amp; Test" />
</manifest>"#;
        let parsed = parse_manifest_print(output).unwrap();
        assert_eq!(
            parsed.split_reason.as_deref(),
            Some("manifest declares split=\"config.en\"")
        );
        assert_eq!(parsed.display_name.as_deref(), Some("Example & Test"));
    }

    #[test]
    fn ignores_resource_label_and_false_feature_split() {
        let output = r#"<manifest android:isFeatureSplit="false"><application android:label="@string/app_name" /></manifest>"#;
        let parsed = parse_manifest_print(output).unwrap();
        assert!(parsed.split_reason.is_none());
        assert!(parsed.display_name.is_none());
    }

    #[test]
    fn rejects_a_base_apk_that_requires_other_splits() {
        let output = r#"<manifest><application android:isSplitRequired="true" /></manifest>"#;
        let parsed = parse_manifest_print(output).unwrap();
        assert_eq!(
            parsed.split_reason.as_deref(),
            Some("application declares isSplitRequired=true")
        );
    }

    #[test]
    fn extracts_only_a_complete_sha256_certificate_digest() {
        let digest = "AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99";
        let output = format!("Signer #1 certificate SHA-256 digest: {digest}\n");
        assert_eq!(extract_certificate_sha256(&output).unwrap(), digest);
        assert!(extract_certificate_sha256("Signer #1 certificate SHA-256 digest: AA:BB").is_err());
    }

    #[test]
    fn accepts_safe_root_zip_names_only() {
        assert!(safe_zip_name("AndroidManifest.xml"));
        assert!(safe_zip_name("res/drawable/icon.png"));
        assert!(!safe_zip_name("../AndroidManifest.xml"));
        assert!(!safe_zip_name("/AndroidManifest.xml"));
        assert!(!safe_zip_name("res//icon.png"));
    }

    #[test]
    fn an_invalid_explicit_tool_override_remains_a_hard_error() {
        let temporary = tempfile::tempdir().unwrap();
        let missing = temporary.path().join("missing-apkanalyzer");
        let error = resolve_optional_tool(
            "apkanalyzer",
            "--apkanalyzer-bin",
            Some(&missing),
            Vec::new(),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("--apkanalyzer-bin"));
        assert!(error.contains("not an executable regular file"));
    }

    #[cfg(unix)]
    #[test]
    fn inspect_composes_structural_checks_and_official_tool_outputs() {
        let temporary = tempfile::tempdir().unwrap();
        let apk_path = temporary.path().join("Example.apk");
        write_structural_apk(&apk_path);
        let analyzer = temporary.path().join("apkanalyzer");
        write_executable(
            &analyzer,
            r#"#!/bin/sh
case "$1 $2" in
  "manifest print") printf '%s\n' '<manifest package="com.example.app"><application android:label="Example" /></manifest>' ;;
  "manifest application-id") printf '%s\n' 'com.example.app' ;;
  "manifest version-code") printf '%s\n' '42' ;;
  "manifest version-name") printf '%s\n' '2.1.0' ;;
  "manifest min-sdk") printf '%s\n' '26' ;;
  "manifest target-sdk") printf '%s\n' '36' ;;
  *) exit 64 ;;
esac
"#,
        );
        let signer = temporary.path().join("apksigner");
        write_executable(
            &signer,
            "#!/bin/sh\nprintf '%s\\n' 'Signer #1 certificate SHA-256 digest: AABBCCDDEEFF00112233445566778899AABBCCDDEEFF00112233445566778899'\n",
        );

        let metadata = inspect(&apk_path, None, &ApkToolchain::new(&analyzer, &signer)).unwrap();
        assert_eq!(metadata.file_name, "Example.apk");
        assert_eq!(metadata.package_name.as_deref(), Some("com.example.app"));
        assert_eq!(metadata.version_code.as_deref(), Some("42"));
        assert_eq!(metadata.version_name.as_deref(), Some("2.1.0"));
        assert_eq!(metadata.min_sdk, Some(26));
        assert_eq!(metadata.target_sdk, Some(36));
        assert_eq!(metadata.display_name.as_deref(), Some("Example"));
        assert_eq!(metadata.certificate_sha256.as_deref().unwrap().len(), 64);
        assert_eq!(metadata.sha256.len(), 64);
        assert!(metadata.warnings.is_empty());
    }

    #[test]
    fn inspect_keeps_structural_checks_and_warns_when_sdk_tools_are_unavailable() {
        let temporary = tempfile::tempdir().unwrap();
        let apk_path = temporary.path().join("Example.apk");
        write_structural_apk(&apk_path);

        let metadata = inspect(
            &apk_path,
            None,
            &ApkToolchain::from_optional_paths(None, None),
        )
        .unwrap();

        assert_eq!(metadata.file_name, "Example.apk");
        assert_eq!(
            metadata.byte_count,
            std::fs::metadata(&apk_path).unwrap().len()
        );
        assert_eq!(metadata.sha256.len(), 64);
        assert!(metadata.package_name.is_none());
        assert!(metadata.version_code.is_none());
        assert!(metadata.certificate_sha256.is_none());
        assert_eq!(metadata.warnings.len(), 2);
        assert!(metadata.warnings[0].contains("without Android manifest metadata"));
        assert!(metadata.warnings[1].contains("without APK signature verification"));
    }

    #[cfg(unix)]
    #[test]
    fn inspect_reports_signature_verification_failure_without_fallback() {
        let temporary = tempfile::tempdir().unwrap();
        let apk_path = temporary.path().join("Unsigned.apk");
        write_structural_apk(&apk_path);
        let analyzer = temporary.path().join("apkanalyzer");
        write_executable(
            &analyzer,
            r#"#!/bin/sh
case "$1 $2" in
  "manifest print") printf '%s\n' '<manifest package="com.example.app"><application /></manifest>' ;;
  "manifest application-id") printf '%s\n' 'com.example.app' ;;
  "manifest version-code") printf '%s\n' '1' ;;
  "manifest version-name"|"manifest min-sdk"|"manifest target-sdk") printf '%s\n' '?' ;;
  *) exit 64 ;;
esac
"#,
        );
        let signer = temporary.path().join("apksigner");
        write_executable(
            &signer,
            "#!/bin/sh\nprintf '%s\\n' 'DOES NOT VERIFY' >&2\nexit 1\n",
        );

        let error = inspect(&apk_path, None, &ApkToolchain::new(&analyzer, &signer))
            .unwrap_err()
            .to_string();
        assert!(error.contains("apksigner"), "{error}");
        assert!(error.contains("DOES NOT VERIFY"), "{error}");
    }
}
