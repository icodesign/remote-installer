use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;
use std::process::Command;

use plist::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use zip::ZipArchive;

use crate::model::MAX_ARTIFACT_BYTES;

const MAX_INFO_PLIST_BYTES: u64 = 4 * 1024 * 1024;
/// Real `embedded.mobileprovision` blobs run to a few tens of kilobytes; this
/// only has to be generous enough to never reject a legitimate one.
const MAX_PROVISIONING_PROFILE_BYTES: u64 = 1024 * 1024;
const MAX_ICON_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ICON_DIMENSION: u32 = 4096;
const MAX_ICON_PIXELS: u64 = 16 * 1024 * 1024;
const PNG_HEADER_BYTES: usize = 24;
// The probe includes a complete CgBI chunk followed by the complete IHDR
// chunk (including its CRC), while remaining bounded before we decide whether
// the candidate is an icon.
const PNG_PROBE_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpaIcon {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// What an IPA carries to show it can actually be installed on a device.
///
/// Read from the archive's entry listing during the walk `inspect` already
/// performs, so gathering it costs no extra pass over a multi-gigabyte file.
/// Kept out of `IpaMetadata` on purpose: the profile holds the developer
/// certificate and the provisioned device UDIDs, and there is no reason to
/// keep that alive for the life of the process or clone it into the service.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SigningEvidence {
    /// Whether the root app bundle carries `_CodeSignature/CodeResources`,
    /// which `codesign` always writes for a signed bundle.
    pub has_code_signature: bool,
    /// The raw CMS-wrapped `embedded.mobileprovision`, when the bundle has one.
    /// An App Store build has none, and cannot be installed over the air.
    pub provisioning_profile: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpaMetadata {
    pub file_name: String,
    pub byte_count: u64,
    pub sha256: String,
    pub bundle_identifier: String,
    pub bundle_version: String,
    pub bundle_short_version: Option<String>,
    pub display_name: Option<String>,
    pub minimum_os_version: Option<String>,
    pub icon: Option<IpaIcon>,
}

#[derive(Debug, Error)]
pub enum IpaError {
    #[error("IPA file I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("IPA archive is invalid: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("IPA property list is invalid: {0}")]
    Plist(#[from] plist::Error),
    #[error("IPA is invalid: {0}")]
    Invalid(String),
}

pub fn inspect(path: &Path, requested_file_name: Option<&str>) -> Result<IpaMetadata, IpaError> {
    inspect_with_signing(path, requested_file_name).map(|(metadata, _)| metadata)
}

/// `inspect`, plus the signing evidence needed to tell whether iOS would even
/// consider installing the archive.
pub fn inspect_with_signing(
    path: &Path,
    requested_file_name: Option<&str>,
) -> Result<(IpaMetadata, SigningEvidence), IpaError> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(IpaError::Invalid("IPA source is not a regular file".into()));
    }
    if metadata.len() == 0 || metadata.len() > MAX_ARTIFACT_BYTES {
        return Err(IpaError::Invalid(format!(
            "IPA must be between 1 byte and {MAX_ARTIFACT_BYTES} bytes"
        )));
    }
    let file_name = normalize_file_name(
        requested_file_name
            .or_else(|| path.file_name().and_then(|value| value.to_str()))
            .ok_or_else(|| IpaError::Invalid("IPA file name is missing".into()))?,
    )?;
    let file = File::open(path)?;
    let mut archive = ZipArchive::new(file)?;
    let mut info_index = None;
    let mut entry_names = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name();
        if !safe_zip_name(name) {
            return Err(IpaError::Invalid(format!("unsafe ZIP entry: {name}")));
        }
        if is_symlink_entry(entry.unix_mode()) {
            return Err(IpaError::Invalid(format!("unsafe ZIP entry: {name}")));
        }
        // A suffix match on ".app/Info.plist" would also match a nested
        // companion bundle's plist (e.g. a watch app at
        // Payload/Main.app/Watch/MainWatch.app/Info.plist, or an app
        // extension at Payload/Main.app/PlugIns/Widget.appex/Info.plist),
        // either mistaking it for the root bundle or tripping the
        // more-than-one-bundle check below. The root app's Info.plist is
        // always exactly three path segments deep, so match that shape
        // instead of a suffix.
        let mut segments = name.split('/');
        let is_root_app_info = matches!(
            (
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
            ),
            (Some("Payload"), Some(app), Some("Info.plist"), None) if app.ends_with(".app")
        );
        if is_root_app_info && info_index.replace(index).is_some() {
            return Err(IpaError::Invalid(
                "IPA Payload contains more than one app bundle".into(),
            ));
        }
        entry_names.push((index, name.to_owned(), entry.size(), entry.is_dir()));
    }
    let info_index = info_index.ok_or_else(|| {
        IpaError::Invalid("IPA does not contain Payload/<App>.app/Info.plist".into())
    })?;
    let mut info_entry = archive.by_index(info_index)?;
    let info_bytes = read_entry_limited(&mut info_entry, MAX_INFO_PLIST_BYTES)?;
    drop(info_entry);
    let info = Value::from_reader(Cursor::new(info_bytes))?;
    let dictionary = info
        .as_dictionary()
        .ok_or_else(|| IpaError::Invalid("Info.plist is not a dictionary".into()))?;
    let bundle_identifier = string_value(dictionary, "CFBundleIdentifier")
        .ok_or_else(|| IpaError::Invalid("CFBundleIdentifier is missing".into()))?;
    let bundle_short_version = string_value(dictionary, "CFBundleShortVersionString");
    let bundle_version = string_value(dictionary, "CFBundleVersion")
        .or_else(|| bundle_short_version.clone())
        .ok_or_else(|| IpaError::Invalid("CFBundleVersion is missing".into()))?;
    let info_name = entry_names
        .iter()
        .find(|(index, _, _, _)| *index == info_index)
        .map(|(_, name, _, _)| name.as_str())
        .ok_or_else(|| IpaError::Invalid("IPA Info.plist entry is missing".into()))?;
    let app_prefix = info_name
        .strip_suffix("Info.plist")
        .ok_or_else(|| IpaError::Invalid("IPA Info.plist path is invalid".into()))?
        .to_owned();
    let signing = signing_evidence(&mut archive, &entry_names, &app_prefix)?;
    let app_prefix = app_prefix.as_str();
    Ok((
        IpaMetadata {
            file_name,
            byte_count: metadata.len(),
            sha256: sha256_file(path)?,
            bundle_identifier,
            bundle_version,
            bundle_short_version,
            display_name: string_value(dictionary, "CFBundleDisplayName")
                .or_else(|| string_value(dictionary, "CFBundleName")),
            minimum_os_version: string_value(dictionary, "MinimumOSVersion"),
            icon: extract_icon(&mut archive, &entry_names, dictionary, app_prefix)?,
        },
        signing,
    ))
}

/// Collect the root bundle's signing evidence from the already-built entry
/// listing, reading only the provisioning profile itself.
fn signing_evidence(
    archive: &mut ZipArchive<File>,
    entries: &[(usize, String, u64, bool)],
    app_prefix: &str,
) -> Result<SigningEvidence, IpaError> {
    let code_resources = format!("{app_prefix}_CodeSignature/CodeResources");
    let profile_name = format!("{app_prefix}embedded.mobileprovision");
    let has_code_signature = entries
        .iter()
        .any(|(_, name, size, is_dir)| !is_dir && *size > 0 && *name == code_resources);
    let profile_index = entries
        .iter()
        .find(|(_, name, size, is_dir)| !is_dir && *size > 0 && *name == profile_name)
        .map(|(index, _, _, _)| *index);
    let provisioning_profile = match profile_index {
        Some(index) => {
            let mut entry = archive.by_index(index)?;
            Some(read_entry_limited(
                &mut entry,
                MAX_PROVISIONING_PROFILE_BYTES,
            )?)
        }
        None => None,
    };
    Ok(SigningEvidence {
        has_code_signature,
        provisioning_profile,
    })
}

pub fn normalize_file_name(value: &str) -> Result<String, IpaError> {
    let trimmed = value.trim();
    let file_name = Path::new(trimmed)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| IpaError::Invalid("IPA file name is invalid".into()))?;
    if !file_name.to_ascii_lowercase().ends_with(".ipa") {
        return Err(IpaError::Invalid("IPA file name must end with .ipa".into()));
    }
    Ok(file_name.to_string())
}

/// URLs for the assets an OTA manifest can advertise.
#[derive(Debug, Clone, Copy, Default)]
pub struct ManifestAssets<'a> {
    pub ipa_url: &'a str,
    /// 57x57 PNG shown on the home screen while the app downloads.
    pub display_image_url: Option<&'a str>,
    /// 512x512 PNG shown in iTunes-style contexts.
    pub full_size_image_url: Option<&'a str>,
}

pub fn manifest_xml(
    bundle_identifier: &str,
    bundle_version: &str,
    title: &str,
    assets: &ManifestAssets<'_>,
) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>items</key><array><dict><key>assets</key><array>{}</array><key>metadata</key><dict><key>bundle-identifier</key><string>{}</string><key>bundle-version</key><string>{}</string><key>kind</key><string>software</string><key>title</key><string>{}</string></dict></dict></array></dict></plist>
"#,
        manifest_assets_xml(assets),
        xml_escape(bundle_identifier),
        xml_escape(bundle_version),
        xml_escape(title),
    )
}

/// Builds the `<array>` contents (without the surrounding tags) for the
/// `assets` key of an OTA manifest: the `software-package` dict is always
/// present and always first, followed by whichever image assets were
/// supplied.
fn manifest_assets_xml(assets: &ManifestAssets<'_>) -> String {
    let mut xml = manifest_asset_dict_xml("software-package", assets.ipa_url);
    if let Some(url) = assets.display_image_url {
        xml.push_str(&manifest_asset_dict_xml("display-image", url));
    }
    if let Some(url) = assets.full_size_image_url {
        xml.push_str(&manifest_asset_dict_xml("full-size-image", url));
    }
    xml
}

fn manifest_asset_dict_xml(kind: &str, url: &str) -> String {
    format!(
        "<dict><key>kind</key><string>{}</string><key>url</key><string>{}</string></dict>",
        xml_escape(kind),
        xml_escape(url),
    )
}

pub fn itms_services_url(manifest_url: &str) -> String {
    format!(
        "itms-services://?action=download-manifest&url={}",
        percent_encode(manifest_url)
    )
}

#[derive(Debug)]
struct IconDeclaration {
    stem: String,
    priority: u8,
    order: usize,
}

#[derive(Debug)]
struct IconCandidate {
    index: usize,
    priority: u8,
    order: usize,
    width: u32,
    height: u32,
    scale: u8,
}

fn extract_icon(
    archive: &mut ZipArchive<File>,
    entries: &[(usize, String, u64, bool)],
    dictionary: &plist::Dictionary,
    app_prefix: &str,
) -> Result<Option<IpaIcon>, IpaError> {
    let declarations = icon_declarations(dictionary);
    if declarations.is_empty() {
        return Ok(None);
    }

    let mut best = None;
    for (index, name, size, is_dir) in entries {
        let Some(leaf) = name.strip_prefix(app_prefix) else {
            continue;
        };
        if *is_dir || leaf.is_empty() || leaf.contains('/') || *size > MAX_ICON_BYTES {
            continue;
        }
        let Some(entry_stem) = icon_stem(leaf) else {
            continue;
        };
        for declaration in &declarations {
            if !icon_name_matches(&entry_stem, &declaration.stem) {
                continue;
            }
            let Some((width, height)) = png_dimensions(archive, *index)? else {
                continue;
            };
            let candidate = IconCandidate {
                index: *index,
                priority: declaration.priority,
                order: declaration.order,
                width,
                height,
                scale: icon_scale(&entry_stem),
            };
            if best
                .as_ref()
                .is_none_or(|current| icon_candidate_is_better(&candidate, current))
            {
                best = Some(candidate);
            }
        }
    }

    best.map(|candidate| read_icon(archive, candidate.index))
        .transpose()
}

fn icon_declarations(dictionary: &plist::Dictionary) -> Vec<IconDeclaration> {
    let mut declarations = Vec::new();
    let mut order = 0;
    collect_modern_icon_files(
        dictionary.get("CFBundleIcons"),
        0,
        &mut order,
        &mut declarations,
    );
    collect_icon_files(
        dictionary.get("CFBundleIconFiles"),
        1,
        &mut order,
        &mut declarations,
    );
    collect_icon_files(
        dictionary.get("CFBundleIconFile"),
        1,
        &mut order,
        &mut declarations,
    );
    collect_modern_icon_files(
        dictionary.get("CFBundleIcons~ipad"),
        2,
        &mut order,
        &mut declarations,
    );
    collect_icon_files(
        dictionary.get("CFBundleIconFiles~ipad"),
        3,
        &mut order,
        &mut declarations,
    );
    collect_icon_files(
        dictionary.get("CFBundleIconFile~ipad"),
        3,
        &mut order,
        &mut declarations,
    );
    declarations
}

fn collect_modern_icon_files(
    value: Option<&Value>,
    priority: u8,
    order: &mut usize,
    declarations: &mut Vec<IconDeclaration>,
) {
    let Some(dictionary) = value.and_then(Value::as_dictionary) else {
        return;
    };
    let primary = dictionary
        .get("CFBundlePrimaryIcon")
        .and_then(Value::as_dictionary)
        .unwrap_or(dictionary);
    collect_icon_files(
        primary.get("CFBundleIconFiles"),
        priority,
        order,
        declarations,
    );
}

fn collect_icon_files(
    value: Option<&Value>,
    priority: u8,
    order: &mut usize,
    declarations: &mut Vec<IconDeclaration>,
) {
    match value {
        Some(Value::Array(values)) => {
            for value in values {
                if let Some(value) = value.as_string() {
                    add_icon_declaration(value, priority, order, declarations);
                }
            }
        }
        Some(Value::String(value)) => add_icon_declaration(value, priority, order, declarations),
        _ => {}
    }
}

fn add_icon_declaration(
    value: &str,
    priority: u8,
    order: &mut usize,
    declarations: &mut Vec<IconDeclaration>,
) {
    let Some(stem) = icon_stem(value) else {
        return;
    };
    declarations.push(IconDeclaration {
        stem,
        priority,
        order: *order,
    });
    *order += 1;
}

fn icon_stem(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.ends_with('/')
        || value.contains('/')
        || value.contains('\\')
        || value.contains("..")
    {
        return None;
    }
    let leaf = value;
    if leaf.is_empty() || leaf.eq_ignore_ascii_case("assets.car") {
        return None;
    }
    let stem = if leaf.len() >= 4 {
        let extension_start = leaf.len() - 4;
        match leaf.get(extension_start..) {
            Some(extension) if extension.eq_ignore_ascii_case(".png") => &leaf[..extension_start],
            _ => leaf,
        }
    } else {
        leaf
    };
    (!stem.is_empty()).then(|| stem.to_string())
}

fn icon_name_matches(entry_stem: &str, declaration_stem: &str) -> bool {
    entry_stem == declaration_stem
        || ["@2x", "@3x"]
            .iter()
            .any(|scale| entry_stem == format!("{declaration_stem}{scale}"))
}

fn icon_scale(stem: &str) -> u8 {
    if stem.ends_with("@3x") {
        3
    } else if stem.ends_with("@2x") {
        2
    } else {
        1
    }
}

fn icon_candidate_is_better(candidate: &IconCandidate, current: &IconCandidate) -> bool {
    candidate.priority < current.priority
        || (candidate.priority == current.priority
            && (u64::from(candidate.width) * u64::from(candidate.height)
                > u64::from(current.width) * u64::from(current.height)
                || (candidate.width == current.width
                    && candidate.height == current.height
                    && (candidate.scale > current.scale
                        || (candidate.scale == current.scale && candidate.order < current.order)))))
}

fn read_entry_limited(entry: &mut zip::read::ZipFile<'_>, limit: u64) -> Result<Vec<u8>, IpaError> {
    if entry.size() > limit {
        return Err(IpaError::Invalid(format!(
            "ZIP entry {} exceeds the {limit}-byte inspection limit",
            entry.name()
        )));
    }
    // `entry.size()` is the uncompressed size recorded in the (attacker
    // controlled) ZIP header, so it is only trusted for the cheap early
    // rejection above and to cap the allocation hint below -- never to
    // bound the read itself. `.take(limit + 1)` bounds the actual read so
    // an entry whose header understates its true decompressed size cannot
    // grow `bytes` past the limit before the post-read check catches it.
    let mut bytes = Vec::with_capacity(entry.size().min(limit) as usize);
    entry
        .by_ref()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(IpaError::Invalid(format!(
            "ZIP entry {} exceeds the {limit}-byte inspection limit",
            entry.name()
        )));
    }
    Ok(bytes)
}

fn png_dimensions(
    archive: &mut ZipArchive<File>,
    index: usize,
) -> Result<Option<(u32, u32)>, IpaError> {
    let mut entry = archive.by_index(index)?;
    if entry.is_dir() || entry.size() < PNG_HEADER_BYTES as u64 || entry.size() > MAX_ICON_BYTES {
        return Ok(None);
    }
    let mut header = [0_u8; PNG_PROBE_BYTES];
    let mut read = 0;
    while read < header.len() {
        let count = entry.read(&mut header[read..])?;
        if count == 0 {
            break;
        }
        read += count;
    }
    Ok(parse_png_dimensions(&header[..read]))
}

fn parse_png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < PNG_HEADER_BYTES || &bytes[..8] != PNG_SIGNATURE {
        return None;
    }
    parse_png_probe(bytes).map(|probe| probe.dimensions)
}

fn parse_ihdr_dimensions(bytes: &[u8], chunk_offset: usize) -> Option<(u32, u32)> {
    // The dimensions occupy the first eight bytes of the IHDR payload.  Do
    // not require the rest of the payload or CRC here: `png_dimensions` only
    // reads a bounded probe rather than the entire icon entry.
    let end = chunk_offset.checked_add(16)?;
    if bytes.len() < end
        || u32::from_be_bytes(bytes[chunk_offset..chunk_offset + 4].try_into().ok()?) != 13
        || &bytes[chunk_offset + 4..chunk_offset + 8] != b"IHDR"
    {
        return None;
    }
    let width = u32::from_be_bytes(bytes[chunk_offset + 8..chunk_offset + 12].try_into().ok()?);
    let height = u32::from_be_bytes(
        bytes[chunk_offset + 12..chunk_offset + 16]
            .try_into()
            .ok()?,
    );
    valid_icon_dimensions(width, height).then_some((width, height))
}

fn valid_icon_dimensions(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= MAX_ICON_DIMENSION
        && height <= MAX_ICON_DIMENSION
        && u64::from(width).saturating_mul(u64::from(height)) <= MAX_ICON_PIXELS
}

fn read_icon(archive: &mut ZipArchive<File>, index: usize) -> Result<IpaIcon, IpaError> {
    let mut entry = archive.by_index(index)?;
    let bytes = read_entry_limited(&mut entry, MAX_ICON_BYTES)?;
    let (width, height) = parse_png_dimensions(&bytes)
        .ok_or_else(|| IpaError::Invalid("selected app icon is not a valid PNG".into()))?;

    if is_cgbi_png(&bytes) {
        let bytes = normalize_cgbi_png(&bytes)?;
        let (normalized_width, normalized_height) = parse_standard_png_dimensions(&bytes)
            .ok_or_else(|| {
                IpaError::Invalid("sips did not produce a standard PNG app icon".into())
            })?;
        if (normalized_width, normalized_height) != (width, height) {
            return Err(IpaError::Invalid(
                "normalized CgBI icon dimensions do not match its source".into(),
            ));
        }
        return Ok(IpaIcon {
            bytes,
            width: normalized_width,
            height: normalized_height,
        });
    }

    Ok(IpaIcon {
        bytes,
        width,
        height,
    })
}

fn parse_standard_png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    parse_png_probe(bytes).and_then(|probe| (!probe.has_cgbi).then_some(probe.dimensions))
}

fn is_cgbi_png(bytes: &[u8]) -> bool {
    parse_png_probe(bytes).is_some_and(|probe| probe.has_cgbi)
}

#[derive(Debug, Clone, Copy)]
struct PngProbe {
    dimensions: (u32, u32),
    has_cgbi: bool,
}

fn parse_png_probe(bytes: &[u8]) -> Option<PngProbe> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < PNG_HEADER_BYTES || &bytes[..8] != PNG_SIGNATURE {
        return None;
    }
    parse_png_chunks(bytes, 8, false)
}

fn parse_png_chunks(bytes: &[u8], mut offset: usize, has_cgbi: bool) -> Option<PngProbe> {
    while offset.checked_add(8)? <= bytes.len() {
        let chunk_length = usize::try_from(u32::from_be_bytes(
            bytes[offset..offset + 4].try_into().ok()?,
        ))
        .ok()?;
        let chunk_type = &bytes[offset + 4..offset + 8];
        let data_start = offset.checked_add(8)?;
        let data_end = data_start.checked_add(chunk_length)?;
        if data_end > bytes.len() {
            return None;
        }

        if chunk_type == b"IHDR" {
            return parse_ihdr_dimensions(bytes, offset).map(|dimensions| PngProbe {
                dimensions,
                has_cgbi,
            });
        }

        if chunk_type == b"CgBI" {
            // CgBI is an Apple-specific pre-IHDR chunk. Most producers emit
            // its CRC, while older pngcrush versions omit it. Try the normal
            // chunk framing first, then retry without the optional CRC.
            if let Some(probe) = parse_png_chunks(bytes, data_end.checked_add(4)?, true) {
                return Some(probe);
            }
            return parse_png_chunks(bytes, data_end, true);
        }

        // PNG chunks carry a four-byte CRC after their payload.
        offset = data_end.checked_add(4)?;
    }
    None
}

#[cfg(target_os = "macos")]
fn normalize_cgbi_png(bytes: &[u8]) -> Result<Vec<u8>, IpaError> {
    if bytes.len() as u64 > MAX_ICON_BYTES {
        return Err(IpaError::Invalid(
            "CgBI icon exceeds the input size limit".into(),
        ));
    }
    let temporary = tempfile::Builder::new()
        .prefix("ipa-cgbi-")
        .tempdir()
        .map_err(|error| {
            IpaError::Invalid(format!("cannot create CgBI temp directory: {error}"))
        })?;
    let input = temporary.path().join("source.png");
    let output = temporary.path().join("normalized.png");
    std::fs::write(&input, bytes)?;
    let result = Command::new("/usr/bin/sips")
        .args(["-s", "format", "png"])
        .arg(&input)
        .arg("--out")
        .arg(&output)
        .output()?;
    if !result.status.success() {
        return Err(IpaError::Invalid(format!(
            "sips could not normalize Apple CgBI icon: {}",
            command_diagnostic(&result)
        )));
    }
    let normalized = read_file_limited(&output, MAX_ICON_BYTES)?;
    if normalized.is_empty() {
        return Err(IpaError::Invalid(
            "sips produced an empty normalized CgBI icon".into(),
        ));
    }
    Ok(normalized)
}

#[cfg(not(target_os = "macos"))]
fn normalize_cgbi_png(_bytes: &[u8]) -> Result<Vec<u8>, IpaError> {
    Err(IpaError::Invalid(
        "Apple CgBI icon normalization requires macOS /usr/bin/sips".into(),
    ))
}

fn read_file_limited(path: &Path, limit: u64) -> Result<Vec<u8>, IpaError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(IpaError::Invalid(
            "icon normalizer output is not a regular file".into(),
        ));
    }
    if metadata.len() > limit {
        return Err(IpaError::Invalid(format!(
            "icon normalizer output exceeds the {limit}-byte limit"
        )));
    }
    let file = File::open(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(IpaError::Invalid(format!(
            "icon normalizer output exceeds the {limit}-byte limit"
        )));
    }
    Ok(bytes)
}

fn command_diagnostic(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim()
    } else if !stdout.trim().is_empty() {
        stdout.trim()
    } else {
        "no diagnostic output"
    };
    detail.chars().take(512).collect()
}

fn string_value(dictionary: &plist::Dictionary, key: &str) -> Option<String> {
    dictionary.get(key)?.as_string().map(ToOwned::to_owned)
}

/// Whether a ZIP entry's Unix file-type bits (`st_mode & S_IFMT`) mark it as
/// a symbolic link. IPAs are extracted with `/usr/bin/ditto`, which honours
/// symlinks, so a symlink entry could point extraction at a path outside
/// the IPA's own contents.
fn is_symlink_entry(unix_mode: Option<u32>) -> bool {
    const S_IFMT: u32 = 0o170000;
    const S_IFLNK: u32 = 0o120000;
    unix_mode.is_some_and(|mode| mode & S_IFMT == S_IFLNK)
}

fn safe_zip_name(name: &str) -> bool {
    let normalized = name.strip_suffix('/').unwrap_or(name);
    !normalized.starts_with('/')
        && !normalized
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn sha256_file(path: &Path) -> Result<String, IpaError> {
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

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_escapes_user_values() {
        let assets = ManifestAssets {
            ipa_url: "https://host/app?a=1&b=2",
            ..Default::default()
        };
        let manifest = manifest_xml("com.example.&app", "1", "A < B", &assets);
        assert!(manifest.contains("com.example.&amp;app"));
        assert!(manifest.contains("A &lt; B"));
        assert!(manifest.contains("a=1&amp;b=2"));
    }

    #[test]
    fn manifest_includes_both_image_assets_after_the_software_package_when_provided() {
        let assets = ManifestAssets {
            ipa_url: "https://host/app.ipa",
            display_image_url: Some("https://host/icon.png"),
            full_size_image_url: Some("https://host/icon.png"),
        };
        let manifest = manifest_xml("com.example.app", "1", "Title", &assets);
        assert!(manifest.contains("<key>kind</key><string>display-image</string>"));
        assert!(manifest.contains("<key>kind</key><string>full-size-image</string>"));

        let software_index = manifest
            .find("<key>kind</key><string>software-package</string>")
            .expect("software-package asset present");
        let display_index = manifest
            .find("<key>kind</key><string>display-image</string>")
            .expect("display-image asset present");
        let full_size_index = manifest
            .find("<key>kind</key><string>full-size-image</string>")
            .expect("full-size-image asset present");
        assert!(software_index < display_index);
        assert!(display_index < full_size_index);
    }

    #[test]
    fn manifest_omits_image_assets_when_urls_are_absent() {
        let assets = ManifestAssets {
            ipa_url: "https://host/app.ipa",
            ..Default::default()
        };
        let manifest = manifest_xml("com.example.app", "1", "Title", &assets);
        assert!(!manifest.contains("display-image"));
        assert!(!manifest.contains("full-size-image"));
        assert!(manifest.contains("<key>kind</key><string>software-package</string>"));
    }

    #[test]
    fn itms_services_url_encodes_the_manifest_url() {
        assert_eq!(
            itms_services_url("https://host.example/manifest.plist?id=1&build=2"),
            "itms-services://?action=download-manifest&url=https%3A%2F%2Fhost.example%2Fmanifest.plist%3Fid%3D1%26build%3D2"
        );
    }

    #[test]
    fn rejects_unsafe_zip_names() {
        assert!(!safe_zip_name("../Payload/App.app/Info.plist"));
        assert!(!safe_zip_name("/Payload/App.app/Info.plist"));
        assert!(safe_zip_name("Payload/"));
        assert!(safe_zip_name("Payload/App.app/Info.plist"));
    }

    #[test]
    fn normalizes_only_leaf_ipa_names() {
        assert_eq!(normalize_file_name("/tmp/Build.ipa").unwrap(), "Build.ipa");
        assert!(normalize_file_name("Build.zip").is_err());
    }

    #[test]
    fn inspect_reads_app_metadata() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
        writer
            .start_file::<_, ()>(
                "Payload/Test.app/Info.plist",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        let plist = plist::Value::Dictionary(
            [
                (
                    "CFBundleIdentifier".to_string(),
                    plist::Value::String("com.example.test".into()),
                ),
                (
                    "CFBundleVersion".to_string(),
                    plist::Value::String("7".into()),
                ),
                (
                    "CFBundleShortVersionString".to_string(),
                    plist::Value::String("2.9.0".into()),
                ),
            ]
            .into_iter()
            .collect(),
        );
        plist.to_writer_xml(&mut writer).unwrap();
        writer.finish().unwrap();
        let result = inspect(temp.path(), Some("Test.ipa")).unwrap();
        assert_eq!(result.bundle_identifier, "com.example.test");
        assert_eq!(result.bundle_version, "7");
        assert_eq!(result.bundle_short_version.as_deref(), Some("2.9.0"));
        assert_eq!(result.file_name, "Test.ipa");
        assert!(result.icon.is_none());
    }

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

    #[cfg(target_os = "macos")]
    fn run_test_command(command: &mut Command) {
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "test image command failed: {}",
            command_diagnostic(&output)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn normalizes_a_real_cgbi_png_to_browser_safe_pixels() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source.ppm");
        let standard = temporary.path().join("standard.png");
        let cgbi = temporary.path().join("optimized.png");
        let normalized = temporary.path().join("normalized.png");
        let standard_bitmap = temporary.path().join("standard.bmp");
        let normalized_bitmap = temporary.path().join("normalized.bmp");

        // A tiny, license-free four-color source makes the pixel comparison
        // sensitive to CgBI's BGRA channel order as well as decompression.
        std::fs::write(
            &source,
            b"P3\n2 2\n255\n255 0 0  0 255 0\n0 0 255  255 255 255\n",
        )
        .unwrap();
        run_test_command(
            Command::new("/usr/bin/sips")
                .args(["-s", "format", "png"])
                .arg(&source)
                .arg("--out")
                .arg(&standard),
        );

        let pngcrush = Command::new("xcrun")
            .args(["--find", "pngcrush"])
            .output()
            .unwrap();
        assert!(pngcrush.status.success(), "Xcode pngcrush is required");
        let pngcrush = String::from_utf8(pngcrush.stdout).unwrap();
        run_test_command(
            Command::new(pngcrush.trim())
                .arg("-iphone")
                .arg(&standard)
                .arg(&cgbi),
        );

        let cgbi_bytes = std::fs::read(&cgbi).unwrap();
        assert!(is_cgbi_png(&cgbi_bytes));
        assert_eq!(parse_png_dimensions(&cgbi_bytes), Some((2, 2)));

        let normalized_bytes = normalize_cgbi_png(&cgbi_bytes).unwrap();
        assert!(!is_cgbi_png(&normalized_bytes));
        assert_eq!(
            parse_standard_png_dimensions(&normalized_bytes),
            Some((2, 2))
        );
        std::fs::write(&normalized, normalized_bytes).unwrap();

        for (input, output) in [
            (&standard, &standard_bitmap),
            (&normalized, &normalized_bitmap),
        ] {
            run_test_command(
                Command::new("/usr/bin/sips")
                    .args(["-s", "format", "bmp"])
                    .arg(input)
                    .arg("--out")
                    .arg(output),
            );
        }
        assert_eq!(
            std::fs::read(standard_bitmap).unwrap(),
            std::fs::read(normalized_bitmap).unwrap()
        );
    }

    #[test]
    fn inspect_extracts_the_highest_resolution_iphone_icon() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
        let icon_files = plist::Value::Array(vec![plist::Value::String("AppIcon60x60".into())]);
        let primary = plist::Value::Dictionary(
            [("CFBundleIconFiles".to_string(), icon_files)]
                .into_iter()
                .collect(),
        );
        let icons = plist::Value::Dictionary(
            [("CFBundlePrimaryIcon".to_string(), primary)]
                .into_iter()
                .collect(),
        );
        let ipad_icons = plist::Value::Dictionary(
            [(
                "CFBundleIconFiles".to_string(),
                plist::Value::Array(vec![plist::Value::String("AppIcon76x76".into())]),
            )]
            .into_iter()
            .collect(),
        );
        let plist = plist::Value::Dictionary(
            [
                (
                    "CFBundleIdentifier".to_string(),
                    plist::Value::String("com.example.test".into()),
                ),
                (
                    "CFBundleVersion".to_string(),
                    plist::Value::String("7".into()),
                ),
                ("CFBundleIcons".to_string(), icons),
                ("CFBundleIcons~ipad".to_string(), ipad_icons),
            ]
            .into_iter()
            .collect(),
        );
        writer
            .start_file::<_, ()>(
                "Payload/Test.app/Info.plist",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        plist.to_writer_xml(&mut writer).unwrap();
        for (name, width) in [
            ("AppIcon60x60.png", 60),
            ("AppIcon60x60@2x.png", 120),
            ("AppIcon60x60@3x.png", 180),
            ("AppIcon76x76@2x.png", 152),
        ] {
            writer
                .start_file::<_, ()>(
                    format!("Payload/Test.app/{name}"),
                    zip::write::FileOptions::default(),
                )
                .unwrap();
            std::io::Write::write_all(&mut writer, &png(width, width)).unwrap();
        }
        writer.finish().unwrap();

        let result = inspect(temp.path(), Some("Test.ipa")).unwrap();
        let icon = result.icon.expect("declared PNG icon");
        assert_eq!((icon.width, icon.height), (180, 180));
        assert_eq!(&icon.bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn inspect_extracts_a_legacy_singular_icon_file() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
        writer
            .start_file::<_, ()>(
                "Payload/Test.app/Info.plist",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        let plist = plist::Value::Dictionary(
            [
                (
                    "CFBundleIdentifier".to_string(),
                    plist::Value::String("com.example.legacy".into()),
                ),
                (
                    "CFBundleVersion".to_string(),
                    plist::Value::String("7".into()),
                ),
                (
                    "CFBundleIconFile".to_string(),
                    plist::Value::String("LegacyIcon.png".into()),
                ),
            ]
            .into_iter()
            .collect(),
        );
        plist.to_writer_xml(&mut writer).unwrap();
        writer
            .start_file::<_, ()>(
                "Payload/Test.app/LegacyIcon@2x.png",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        std::io::Write::write_all(&mut writer, &png(120, 120)).unwrap();
        writer.finish().unwrap();

        let result = inspect(temp.path(), Some("Legacy.ipa")).unwrap();
        let icon = result.icon.expect("legacy singular icon");
        assert_eq!((icon.width, icon.height), (120, 120));
    }

    #[test]
    fn inspect_skips_invalid_or_oversized_icon_candidates() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
        writer
            .start_file::<_, ()>(
                "Payload/Test.app/Info.plist",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        let icon_files = plist::Value::Array(
            [
                plist::Value::String("../EscapedIcon".into()),
                plist::Value::String("nested/Icon".into()),
                plist::Value::String(r"nested\Icon".into()),
                plist::Value::String("InvalidPng".into()),
                plist::Value::String("OversizedIcon".into()),
            ]
            .into_iter()
            .collect(),
        );
        let plist = plist::Value::Dictionary(
            [
                (
                    "CFBundleIdentifier".to_string(),
                    plist::Value::String("com.example.invalid-icons".into()),
                ),
                (
                    "CFBundleVersion".to_string(),
                    plist::Value::String("7".into()),
                ),
                ("CFBundleIconFiles".to_string(), icon_files),
            ]
            .into_iter()
            .collect(),
        );
        plist.to_writer_xml(&mut writer).unwrap();
        writer
            .start_file::<_, ()>(
                "Payload/Test.app/InvalidPng.png",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        std::io::Write::write_all(&mut writer, b"not a PNG").unwrap();
        writer
            .start_file::<_, ()>(
                "Payload/Test.app/OversizedIcon.png",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        std::io::Write::write_all(&mut writer, &vec![0_u8; (MAX_ICON_BYTES + 1) as usize]).unwrap();
        writer.finish().unwrap();

        let result = inspect(temp.path(), Some("Invalid.ipa")).unwrap();
        assert!(result.icon.is_none());
        assert!(icon_stem("nested/Icon").is_none());
        assert!(icon_stem(r"nested\Icon").is_none());
    }

    #[test]
    fn inspect_leaves_assets_car_only_icons_unavailable() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
        writer
            .start_file::<_, ()>(
                "Payload/Test.app/Info.plist",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        let plist = plist::Value::Dictionary(
            [
                (
                    "CFBundleIdentifier".to_string(),
                    plist::Value::String("com.example.test".into()),
                ),
                (
                    "CFBundleVersion".to_string(),
                    plist::Value::String("7".into()),
                ),
                (
                    "CFBundleIcons".to_string(),
                    plist::Value::Dictionary(
                        [(
                            "CFBundlePrimaryIcon".to_string(),
                            plist::Value::Dictionary(
                                [(
                                    "CFBundleIconName".to_string(),
                                    plist::Value::String("AppIcon".into()),
                                )]
                                .into_iter()
                                .collect(),
                            ),
                        )]
                        .into_iter()
                        .collect(),
                    ),
                ),
            ]
            .into_iter()
            .collect(),
        );
        plist.to_writer_xml(&mut writer).unwrap();
        writer
            .start_file::<_, ()>(
                "Payload/Test.app/Assets.car",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        std::io::Write::write_all(&mut writer, b"asset catalog").unwrap();
        writer.finish().unwrap();

        let result = inspect(temp.path(), Some("Test.ipa")).unwrap();
        assert!(result.icon.is_none());
    }

    #[test]
    fn inspect_reads_minimum_os_version_when_present_and_none_when_absent() {
        fn build_ipa(minimum_os_version: Option<&str>) -> tempfile::NamedTempFile {
            let temp = tempfile::NamedTempFile::new().unwrap();
            let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
            writer
                .start_file::<_, ()>(
                    "Payload/Test.app/Info.plist",
                    zip::write::FileOptions::default(),
                )
                .unwrap();
            let mut entries = vec![
                (
                    "CFBundleIdentifier".to_string(),
                    plist::Value::String("com.example.test".into()),
                ),
                (
                    "CFBundleVersion".to_string(),
                    plist::Value::String("7".into()),
                ),
            ];
            if let Some(version) = minimum_os_version {
                entries.push((
                    "MinimumOSVersion".to_string(),
                    plist::Value::String(version.into()),
                ));
            }
            let plist = plist::Value::Dictionary(entries.into_iter().collect());
            plist.to_writer_xml(&mut writer).unwrap();
            writer.finish().unwrap();
            temp
        }

        let with_version = build_ipa(Some("14.0"));
        let result = inspect(with_version.path(), Some("Test.ipa")).unwrap();
        assert_eq!(result.minimum_os_version.as_deref(), Some("14.0"));

        let without_version = build_ipa(None);
        let result = inspect(without_version.path(), Some("Test.ipa")).unwrap();
        assert!(result.minimum_os_version.is_none());
    }

    #[test]
    fn read_entry_limited_rejects_an_entry_larger_than_the_limit() {
        // The `zip` writer derives each entry's declared uncompressed size
        // from the bytes actually written, so there is no supported way
        // (short of hand-crafting raw ZIP bytes) to forge a header that
        // understates an entry's true decompressed size. This instead
        // exercises the bounded read path against content that genuinely
        // exceeds the limit, confirming the read is rejected rather than
        // grown without bound.
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
        writer
            .start_file::<_, ()>("large.bin", zip::write::FileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut writer, &vec![0_u8; 4096]).unwrap();
        writer.finish().unwrap();

        let file = File::open(temp.path()).unwrap();
        let mut archive = ZipArchive::new(file).unwrap();
        let mut entry = archive.by_index(0).unwrap();
        let error = read_entry_limited(&mut entry, 1024).unwrap_err();
        assert!(
            matches!(error, IpaError::Invalid(message) if message.contains("exceeds the 1024-byte inspection limit"))
        );
    }

    #[test]
    fn inspect_rejects_symlink_entries() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
        writer
            .add_symlink::<_, _, ()>(
                "Payload/Test.app/Evil",
                "/etc/passwd",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        writer
            .start_file::<_, ()>(
                "Payload/Test.app/Info.plist",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        let plist = plist::Value::Dictionary(
            [
                (
                    "CFBundleIdentifier".to_string(),
                    plist::Value::String("com.example.test".into()),
                ),
                (
                    "CFBundleVersion".to_string(),
                    plist::Value::String("7".into()),
                ),
            ]
            .into_iter()
            .collect(),
        );
        plist.to_writer_xml(&mut writer).unwrap();
        writer.finish().unwrap();

        let error = inspect(temp.path(), Some("Test.ipa")).unwrap_err();
        assert!(
            matches!(error, IpaError::Invalid(message) if message.contains("unsafe ZIP entry"))
        );
    }

    fn write_plist(
        writer: &mut zip::ZipWriter<File>,
        path: &str,
        bundle_identifier: &str,
        extra: Vec<(String, plist::Value)>,
    ) {
        writer
            .start_file::<_, ()>(path, zip::write::FileOptions::default())
            .unwrap();
        let mut entries = vec![
            (
                "CFBundleIdentifier".to_string(),
                plist::Value::String(bundle_identifier.into()),
            ),
            (
                "CFBundleVersion".to_string(),
                plist::Value::String("7".into()),
            ),
        ];
        entries.extend(extra);
        plist::Value::Dictionary(entries.into_iter().collect())
            .to_writer_xml(writer)
            .unwrap();
    }

    #[test]
    fn inspect_accepts_a_watch_companion_app_and_describes_the_root_bundle() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
        write_plist(
            &mut writer,
            "Payload/Main.app/Info.plist",
            "com.example.root",
            vec![],
        );
        // A watchOS companion's Info.plist also ends with ".app/Info.plist",
        // which is exactly the shape the fixed root-bundle match must not be
        // fooled by.
        write_plist(
            &mut writer,
            "Payload/Main.app/Watch/MainWatch.app/Info.plist",
            "com.example.root.watchkitapp",
            vec![],
        );
        writer.finish().unwrap();

        let result = inspect(temp.path(), Some("Main.ipa")).unwrap();
        assert_eq!(result.bundle_identifier, "com.example.root");
    }

    #[test]
    fn inspect_accepts_an_app_extension_and_describes_the_root_bundle() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
        write_plist(
            &mut writer,
            "Payload/Main.app/Info.plist",
            "com.example.root",
            vec![],
        );
        write_plist(
            &mut writer,
            "Payload/Main.app/PlugIns/Widget.appex/Info.plist",
            "com.example.root.widget",
            vec![],
        );
        writer.finish().unwrap();

        let result = inspect(temp.path(), Some("Main.ipa")).unwrap();
        assert_eq!(result.bundle_identifier, "com.example.root");
    }

    #[test]
    fn inspect_rejects_two_genuine_root_app_bundles() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
        write_plist(
            &mut writer,
            "Payload/A.app/Info.plist",
            "com.example.a",
            vec![],
        );
        write_plist(
            &mut writer,
            "Payload/B.app/Info.plist",
            "com.example.b",
            vec![],
        );
        writer.finish().unwrap();

        let error = inspect(temp.path(), Some("Test.ipa")).unwrap_err();
        assert!(
            matches!(error, IpaError::Invalid(message) if message.contains("more than one app bundle"))
        );
    }

    #[test]
    fn inspect_prefers_the_root_apps_icon_over_a_same_named_watch_app_icon() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
        let icon_files = plist::Value::Array(vec![plist::Value::String("AppIcon60x60".into())]);
        write_plist(
            &mut writer,
            "Payload/Main.app/Info.plist",
            "com.example.root",
            vec![("CFBundleIconFiles".to_string(), icon_files)],
        );
        writer
            .start_file::<_, ()>(
                "Payload/Main.app/AppIcon60x60.png",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        std::io::Write::write_all(&mut writer, &png(60, 60)).unwrap();

        write_plist(
            &mut writer,
            "Payload/Main.app/Watch/MainWatch.app/Info.plist",
            "com.example.root.watchkitapp",
            vec![],
        );
        // Same leaf name as the root app's icon, but nested under the watch
        // app's own directory -- and at a different resolution -- so this
        // only resolves to the root app's copy if `app_prefix` still scopes
        // icon lookup to `Payload/Main.app/` alone.
        writer
            .start_file::<_, ()>(
                "Payload/Main.app/Watch/MainWatch.app/AppIcon60x60.png",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        std::io::Write::write_all(&mut writer, &png(999, 999)).unwrap();
        writer.finish().unwrap();

        let result = inspect(temp.path(), Some("Main.ipa")).unwrap();
        assert_eq!(result.bundle_identifier, "com.example.root");
        let icon = result.icon.expect("root app icon");
        assert_eq!((icon.width, icon.height), (60, 60));
    }

    /// Signing evidence must describe the *root* bundle. A nested watch app
    /// carries its own `_CodeSignature` and profile, and counting those would
    /// let an unsigned host app pass as signed.
    #[test]
    fn signing_evidence_is_scoped_to_the_root_app_bundle() {
        fn build(entries: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
            let temp = tempfile::NamedTempFile::new().unwrap();
            let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
            writer
                .start_file::<_, ()>(
                    "Payload/Main.app/Info.plist",
                    zip::write::FileOptions::default(),
                )
                .unwrap();
            plist::Value::Dictionary(
                [
                    (
                        "CFBundleIdentifier".to_string(),
                        Value::String("com.example.main".into()),
                    ),
                    ("CFBundleVersion".to_string(), Value::String("1".into())),
                ]
                .into_iter()
                .collect(),
            )
            .to_writer_xml(&mut writer)
            .unwrap();
            for (name, bytes) in entries {
                writer
                    .start_file::<_, ()>(*name, zip::write::FileOptions::default())
                    .unwrap();
                std::io::Write::write_all(&mut writer, bytes).unwrap();
            }
            writer.finish().unwrap();
            temp
        }

        let bare = build(&[]);
        let (_, signing) = inspect_with_signing(bare.path(), Some("Main.ipa")).unwrap();
        assert!(!signing.has_code_signature);
        assert_eq!(signing.provisioning_profile, None);

        // Only the watch app is signed; the root bundle still is not.
        let nested_only = build(&[
            (
                "Payload/Main.app/Watch/W.app/_CodeSignature/CodeResources",
                b"plist",
            ),
            (
                "Payload/Main.app/Watch/W.app/embedded.mobileprovision",
                b"profile",
            ),
        ]);
        let (_, signing) = inspect_with_signing(nested_only.path(), Some("Main.ipa")).unwrap();
        assert!(!signing.has_code_signature);
        assert_eq!(signing.provisioning_profile, None);

        let signed = build(&[
            ("Payload/Main.app/_CodeSignature/CodeResources", b"plist"),
            (
                "Payload/Main.app/embedded.mobileprovision",
                b"profile-bytes",
            ),
        ]);
        let (_, signing) = inspect_with_signing(signed.path(), Some("Main.ipa")).unwrap();
        assert!(signing.has_code_signature);
        assert_eq!(
            signing.provisioning_profile.as_deref(),
            Some(b"profile-bytes".as_slice())
        );
    }

    /// An empty `_CodeSignature/CodeResources` is not a signature.
    #[test]
    fn signing_evidence_ignores_zero_length_entries() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = zip::ZipWriter::new(temp.reopen().unwrap());
        writer
            .start_file::<_, ()>(
                "Payload/Main.app/Info.plist",
                zip::write::FileOptions::default(),
            )
            .unwrap();
        plist::Value::Dictionary(
            [
                (
                    "CFBundleIdentifier".to_string(),
                    Value::String("com.example.main".into()),
                ),
                ("CFBundleVersion".to_string(), Value::String("1".into())),
            ]
            .into_iter()
            .collect(),
        )
        .to_writer_xml(&mut writer)
        .unwrap();
        for name in [
            "Payload/Main.app/_CodeSignature/CodeResources",
            "Payload/Main.app/embedded.mobileprovision",
        ] {
            writer
                .start_file::<_, ()>(name, zip::write::FileOptions::default())
                .unwrap();
        }
        writer.finish().unwrap();

        let (_, signing) = inspect_with_signing(temp.path(), Some("Main.ipa")).unwrap();
        assert!(!signing.has_code_signature);
        assert_eq!(signing.provisioning_profile, None);
    }
}
