use crate::model::{Artifact, Availability, PlatformMetadata, format_bytes};

/// Render the public install page for an artifact.
///
/// The install page intentionally contains no JavaScript or external assets:
/// the only action is the platform install URL produced by the service.
///
/// A share that can no longer install still renders the whole page, with the
/// button replaced by the reason. Answering 410 instead would leave someone
/// whose install just failed staring at a browser error page, unable to tell
/// which build it was or whether they were even at the right link.
pub fn render(
    artifact: &Artifact,
    install_action_url: &str,
    icon_url: Option<&str>,
    availability: Availability,
) -> String {
    let display_name = html_escape(artifact.title());
    let (
        identifier_label,
        identifier,
        compatibility_markup,
        install_guidance_markup,
        package_kind,
        cta_label,
        install_aria,
    ) = match &artifact.platform_metadata {
        PlatformMetadata::Ios(metadata) => {
            let compatibility = metadata
                .minimum_os_version
                .as_deref()
                .map(str::trim)
                .filter(|version| !version.is_empty())
                .map(|version| {
                    compatibility_markup(&format!("Requires iOS {} or later", html_escape(version)))
                })
                .unwrap_or_default();
            (
                "Bundle ID",
                html_escape(&metadata.bundle_identifier),
                compatibility,
                String::new(),
                "IPA",
                "Install",
                format!("Install {display_name} on this iPhone or iPad"),
            )
        }
        PlatformMetadata::Android(metadata) => {
            let compatibility = metadata
                .min_sdk
                .map(|api| compatibility_markup(&format!("Requires Android API {api} or later")))
                .unwrap_or_default();
            (
                    "Package name",
                    html_escape(metadata.package_name.as_deref().unwrap_or("Unavailable")),
                    compatibility,
                "<p class=\"install-guidance\">After downloading, open the APK. Android may ask you to allow installs from this browser.</p>".into(),
                "APK",
                "Download APK",
                format!("Download {display_name} for Android"),
                )
        }
    };
    let version = html_escape(&version_label(artifact));
    let size = html_escape(&format_bytes(artifact.byte_count));
    let sha256 = html_escape(&abbreviate_sha256(&artifact.sha256));
    let install_action_url = html_escape(install_action_url);
    let icon_markup = match icon_url {
        Some(icon_url) => format!(
            "<div class=\"install-icon\"><img class=\"install-icon-image\" src=\"{}\" alt=\"{} app icon\" width=\"92\" height=\"92\" decoding=\"async\"></div>",
            html_escape(icon_url),
            display_name,
        ),
        None => "<div class=\"install-icon install-icon-missing\" role=\"img\" aria-label=\"App icon unavailable\"></div>".into(),
    };
    let cta_markup = match availability {
        Availability::Installable => format!(
            r#"<a class="install-cta" href="{install_action_url}" aria-label="{install_aria}">{cta_label}</a>"#
        ),
        Availability::LimitReached => notice_markup(
            "Download limit reached",
            "This link has already been used as many times as it allows. Ask for a new one.",
        ),
        Availability::Expired => notice_markup(
            "Link expired",
            "This share link is no longer active. Ask for a new one.",
        ),
    };

    format!(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
  <meta name="theme-color" content="#ffffff" media="(prefers-color-scheme: light)">
  <meta name="theme-color" content="#18181b" media="(prefers-color-scheme: dark)">
  <title>Install {display_name} · {package_kind}</title>
  <style>
    :root {{
      color-scheme: light;
      --page-background: #f4f4f5;
      --surface: #ffffff;
      --surface-sunken: #f4f4f5;
      --surface-panel: rgba(255, 255, 255, 0.62);
      --text-primary: #18181b;
      --text-tertiary: #a1a1aa;
      --border: rgba(24, 24, 27, 0.1);
      --cta: #18181b;
      --cta-hover: #27272a;
      --cta-active: #3f3f46;
      --cta-text: #ffffff;
      --focus: #18181b;
      --focus-ring: rgba(24, 24, 27, 0.2);
      --shadow-card: 0 20px 44px -10px rgba(24, 24, 27, 0.12),
        0 2px 6px rgba(24, 24, 27, 0.04);
      --shadow-icon: 0 4px 14px -4px rgba(24, 24, 27, 0.07),
        0 1px 2px rgba(24, 24, 27, 0.03),
        inset 0 1px 0 rgba(255, 255, 255, 0.5);
      --shadow-panel-inset: inset 0 1px 0 rgba(255, 255, 255, 0.5);
      --font-sans: -apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI", sans-serif;
      --font-mono: ui-monospace, "SFMono-Regular", Menlo, Monaco, Consolas, monospace;
    }}

    @media (prefers-color-scheme: dark) {{
      :root {{
        color-scheme: dark;
        --page-background: #09090b;
        --surface: #18181b;
        --surface-sunken: #0e0e10;
        --surface-panel: rgba(39, 39, 42, 0.72);
        --text-primary: #fafafa;
        --text-tertiary: #a1a1aa;
        --border: rgba(255, 255, 255, 0.14);
        --cta: #fafafa;
        --cta-hover: #ffffff;
        --cta-active: #e4e4e7;
        --cta-text: #18181b;
        --focus: #ffffff;
        --focus-ring: rgba(255, 255, 255, 0.28);
        --shadow-card: 0 20px 44px -10px rgba(0, 0, 0, 0.62),
          0 2px 6px rgba(0, 0, 0, 0.42);
        --shadow-icon: 0 4px 14px -4px rgba(0, 0, 0, 0.5),
          0 1px 2px rgba(0, 0, 0, 0.4),
          inset 0 1px 0 rgba(255, 255, 255, 0.1);
        --shadow-panel-inset: inset 0 1px 0 rgba(255, 255, 255, 0.08);
      }}
    }}

    *, *::before, *::after {{ box-sizing: border-box; }}

    html {{
      min-height: 100%;
      background: var(--page-background);
    }}

    body {{
      min-width: 0;
      min-height: 100vh;
      min-height: 100dvh;
      margin: 0;
      overflow-x: hidden;
      background: var(--page-background);
      color: var(--text-primary);
      font-family: var(--font-sans);
      -webkit-font-smoothing: antialiased;
      text-rendering: optimizeLegibility;
    }}

    .install-page {{
      display: flex;
      min-height: 100vh;
      min-height: 100dvh;
      align-items: center;
      justify-content: center;
      padding: 40px 20px;
    }}

    .install-card {{
      display: flex;
      width: min(390px, 100%);
      min-width: 0;
      flex-direction: column;
      overflow: hidden;
      border: 1px solid var(--border);
      border-radius: 22px;
      background: var(--surface);
      box-shadow: var(--shadow-card);
    }}

    .install-hero {{
      display: flex;
      flex-direction: column;
      align-items: center;
      padding: 40px 24px 32px;
      background: var(--surface-sunken);
      text-align: center;
    }}

    .install-icon {{
      display: flex;
      width: 92px;
      height: 92px;
      align-items: center;
      justify-content: center;
      flex: 0 0 auto;
      border: 1px solid var(--border);
      border-radius: 16px;
      overflow: hidden;
      background: var(--surface-panel);
      box-shadow: var(--shadow-icon);
      color: var(--text-tertiary);
      font-family: var(--font-mono);
      font-size: 11px;
      line-height: 1.45;
      text-align: center;
      backdrop-filter: blur(14px);
    }}

    .install-icon-image {{
      display: block;
      width: 100%;
      height: 100%;
      border-radius: 15px;
      object-fit: cover;
    }}

    .install-title {{
      max-width: 100%;
      margin: 18px 0 0;
      overflow-wrap: anywhere;
      font-size: 26px;
      font-weight: 600;
      letter-spacing: -0.01em;
      line-height: 1.2;
      text-wrap: pretty;
    }}

    .install-subtitle {{
      max-width: 100%;
      margin: 2px 0 0;
      overflow-wrap: anywhere;
      color: var(--text-tertiary);
      font-size: 13px;
      line-height: 1.5;
      text-wrap: pretty;
    }}

    .install-content {{
      min-width: 0;
      padding: 24px 24px 0;
    }}

    .install-compatibility {{
      display: flex;
      min-width: 0;
      align-items: center;
      gap: 8px;
      margin: 0 0 20px;
      padding: 12px 16px;
      border: 1px solid var(--border);
      border-radius: 10px;
      background: var(--surface-panel);
      box-shadow: var(--shadow-panel-inset);
      color: var(--text-primary);
      font-family: var(--font-mono);
      font-size: 12px;
      line-height: 1.5;
      backdrop-filter: blur(14px);
    }}

    .install-info {{
      display: block;
      width: 15px;
      height: 15px;
      flex: 0 0 15px;
      color: var(--text-tertiary);
    }}

    .install-compatibility-text {{
      min-width: 0;
      overflow-wrap: anywhere;
    }}

    .install-guidance {{
      margin: -10px 0 20px;
      color: var(--text-tertiary);
      font-size: 13px;
      line-height: 1.45;
      text-align: center;
    }}

    .install-details {{
      min-width: 0;
      padding: 4px 16px;
      border: 1px solid var(--border);
      border-radius: 10px;
    }}

    .install-row {{
      display: flex;
      min-width: 0;
      align-items: baseline;
      justify-content: space-between;
      gap: 16px;
      padding: 11px 0;
      border-bottom: 1px solid var(--border);
    }}

    .install-row:last-child {{ border-bottom: 0; }}

    .install-label {{
      min-width: 0;
      color: var(--text-tertiary);
      font-size: 13px;
      line-height: 1.5;
    }}

    .install-value {{
      min-width: 0;
      max-width: 70%;
      overflow-wrap: anywhere;
      color: var(--text-primary);
      font-family: var(--font-mono);
      font-size: 13px;
      line-height: 1.5;
      text-align: right;
      word-break: break-word;
    }}

    .install-cta-wrap {{
      padding: 24px 24px 28px;
    }}

    .install-cta {{
      display: flex;
      width: 100%;
      min-height: 48px;
      align-items: center;
      justify-content: center;
      border-radius: 10px;
      background: var(--cta);
      color: var(--cta-text);
      font-size: 17px;
      font-weight: 500;
      line-height: 1.35;
      text-decoration: none;
      transition: background-color 180ms ease, transform 120ms ease, box-shadow 180ms ease;
      -webkit-tap-highlight-color: transparent;
    }}

    .install-cta:hover {{
      background: var(--cta-hover);
      color: var(--cta-text);
      text-decoration: none;
    }}

    .install-cta:active {{
      background: var(--cta-active);
      transform: scale(0.98);
    }}

    .install-notice {{
      display: flex;
      min-height: 48px;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      gap: 4px;
      margin: 0;
      padding: 12px 16px;
      border: 1px solid var(--border);
      border-radius: 10px;
      background: var(--surface-panel);
      box-shadow: var(--shadow-panel-inset);
      text-align: center;
    }}

    .install-notice-heading {{
      color: var(--text-primary);
      font-size: 15px;
      font-weight: 600;
      line-height: 1.35;
    }}

    .install-notice-detail {{
      color: var(--text-tertiary);
      font-size: 13px;
      line-height: 1.5;
    }}

    .install-cta:focus-visible {{
      outline: 3px solid var(--focus);
      outline-offset: 3px;
      box-shadow: 0 0 0 6px var(--focus-ring);
    }}

    @media (max-width: 480px) {{
      html, body {{ background: var(--surface); }}

      .install-page {{
        align-items: stretch;
        justify-content: stretch;
        padding: 0;
        background: var(--surface);
      }}

      .install-card {{
        width: 100%;
        min-height: 100dvh;
        overflow: visible;
        border: 0;
        border-radius: 0;
        box-shadow: none;
      }}

      .install-hero {{
        background: var(--surface);
        padding: calc(32px + env(safe-area-inset-top, 0px))
          calc(20px + env(safe-area-inset-right, 0px)) 28px
          calc(20px + env(safe-area-inset-left, 0px));
      }}

      .install-content {{
        padding: 20px calc(16px + env(safe-area-inset-right, 0px)) 0
          calc(16px + env(safe-area-inset-left, 0px));
      }}

      .install-compatibility {{ margin-bottom: 16px; padding: 12px; }}

      .install-details {{ padding-right: 12px; padding-left: 12px; }}

      .install-row {{ align-items: flex-start; gap: 12px; }}

      .install-label {{ flex: 0 0 35%; }}

      .install-value {{ max-width: 65%; }}

      .install-cta-wrap {{
        margin-top: auto;
        padding: 20px calc(16px + env(safe-area-inset-right, 0px))
          calc(20px + env(safe-area-inset-bottom, 0px))
          calc(16px + env(safe-area-inset-left, 0px));
      }}
    }}

    @media (max-height: 640px) and (min-width: 481px) {{
      .install-page {{ align-items: flex-start; }}
    }}

    @media (prefers-reduced-motion: reduce) {{
      *, *::before, *::after {{
        scroll-behavior: auto !important;
        animation-duration: 0.01ms !important;
        animation-iteration-count: 1 !important;
        transition-duration: 0.01ms !important;
      }}
    }}
  </style>
</head>
<body>
  <main class="install-page">
    <article class="install-card" aria-labelledby="install-title">
      <header class="install-hero">
        {icon_markup}
        <h1 id="install-title" class="install-title">{display_name}</h1>
        <p class="install-subtitle">Preview Build</p>
      </header>

      <div class="install-content">
        {compatibility_markup}
        {install_guidance_markup}

        <dl class="install-details">
          <div class="install-row">
            <dt class="install-label">{identifier_label}</dt>
            <dd class="install-value">{identifier}</dd>
          </div>
          <div class="install-row">
            <dt class="install-label">Version</dt>
            <dd class="install-value">{version}</dd>
          </div>
          <div class="install-row">
            <dt class="install-label">Size</dt>
            <dd class="install-value">{size}</dd>
          </div>
          <div class="install-row">
            <dt class="install-label">SHA-256</dt>
            <dd class="install-value">{sha256}</dd>
          </div>
        </dl>
      </div>

      <div class="install-cta-wrap">
        {cta_markup}
      </div>
    </article>
  </main>
</body>
</html>"##,
    )
}

/// The call-to-action slot when the link can no longer install anything. It
/// occupies the same space as the Install button so the page does not reflow
/// into something unrecognisable.
fn notice_markup(heading: &str, detail: &str) -> String {
    format!(
        "<p class=\"install-notice\"><strong class=\"install-notice-heading\">{}</strong><span class=\"install-notice-detail\">{}</span></p>",
        html_escape(heading),
        html_escape(detail),
    )
}

fn compatibility_markup(message: &str) -> String {
    format!(
        r##"<p class="install-compatibility">
          <svg class="install-info" viewBox="0 0 24 24" fill="none" aria-hidden="true">
            <circle cx="12" cy="12" r="10" stroke="currentColor" stroke-width="2"></circle>
            <line x1="12" y1="11" x2="12" y2="16" stroke="currentColor" stroke-width="2" stroke-linecap="round"></line>
            <circle cx="12" cy="8" r="1" fill="currentColor"></circle>
          </svg>
          <span class="install-compatibility-text">{message}</span>
        </p>"##
    )
}

fn version_label(artifact: &Artifact) -> String {
    let (build, display) = match &artifact.platform_metadata {
        PlatformMetadata::Ios(metadata) => (
            Some(metadata.bundle_version.as_str()),
            metadata.bundle_short_version.as_deref(),
        ),
        PlatformMetadata::Android(metadata) => (
            metadata.version_code.as_deref(),
            metadata.version_name.as_deref(),
        ),
    };
    let build = build.map(str::trim).filter(|version| !version.is_empty());
    let short = display.map(str::trim).filter(|version| !version.is_empty());

    match (short, build) {
        (Some(short), None) => short.to_string(),
        (Some(short), Some(build)) if short == build => short.to_string(),
        (Some(short), Some(build)) => format!("{short} ({build})"),
        (None, Some(build)) => build.to_string(),
        (None, None) => "Unavailable".into(),
    }
}

fn abbreviate_sha256(value: &str) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    if characters.len() <= 12 {
        return value.to_string();
    }

    let prefix = characters.iter().take(6).collect::<String>();
    let suffix = characters.iter().rev().take(6).rev().collect::<String>();
    format!("{prefix}…{suffix}")
}

fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact() -> Artifact {
        Artifact {
            id: "artifact-1".into(),
            file_name: "Console.ipa".into(),
            byte_count: 214_600_000,
            sha256: "7c9e2a1234567890abcdef5fb81d".into(),
            display_name: Some("Console".into()),
            platform_metadata: PlatformMetadata::Ios(crate::model::IosMetadata {
                bundle_identifier: "com.rootstudio.console".into(),
                bundle_version: "1142".into(),
                bundle_short_version: Some("2.9.0".into()),
                minimum_os_version: Some("16.0".into()),
            }),
            has_icon: false,
        }
    }

    fn ios_metadata_mut(artifact: &mut Artifact) -> &mut crate::model::IosMetadata {
        let PlatformMetadata::Ios(metadata) = &mut artifact.platform_metadata else {
            panic!("test artifact should be iOS");
        };
        metadata
    }

    #[test]
    fn render_escapes_dynamic_text_and_url() {
        let mut artifact = artifact();
        artifact.display_name = Some("<Console & \"Preview\">".into());
        ios_metadata_mut(&mut artifact).bundle_identifier = "com.example/a'&\"<".into();
        let html = render(
            &artifact,
            "itms-services://?action=download-manifest&url=https://example.test/m?a=\"x\"",
            None,
            Availability::Installable,
        );

        assert!(html.contains("&lt;Console &amp; &quot;Preview&quot;&gt;"));
        assert!(html.contains("com.example/a&#39;&amp;&quot;&lt;"));
        assert!(html.contains(
            "itms-services://?action=download-manifest&amp;url=https://example.test/m?a=&quot;x&quot;"
        ));
        assert!(!html.contains("<Console & \"Preview\">"));
    }

    #[test]
    fn render_uses_a_safe_icon_url_and_reports_missing_icons_without_a_fake_image() {
        let html = render(
            &artifact(),
            "itms-services://?action=download-manifest&url=https://example.test/manifest.plist",
            Some("/api/v1/artifacts/artifact-1/icon.png?size=92&mode=\"fit\""),
            Availability::Installable,
        );

        assert!(html.contains(
            "src=\"/api/v1/artifacts/artifact-1/icon.png?size=92&amp;mode=&quot;fit&quot;\""
        ));
        assert!(html.contains("class=\"install-icon-image\""));
        assert!(html.contains("alt=\"Console app icon\""));
        assert!(!html.contains("App<br>图标"));

        let without_icon = render(
            &artifact(),
            "itms-services://example.test/install",
            None,
            Availability::Installable,
        );
        assert!(without_icon.contains("install-icon-missing"));
        assert!(without_icon.contains("App icon unavailable"));
        assert!(!without_icon.contains("App<br>图标"));
    }

    #[test]
    fn render_states_minimum_os_version_truthfully_and_omits_the_claim_when_unknown() {
        let mut with_min_os = artifact();
        ios_metadata_mut(&mut with_min_os).minimum_os_version = Some("16.0".into());
        let html = render(
            &with_min_os,
            "itms-services://example.test/install",
            None,
            Availability::Installable,
        );
        assert!(html.contains("Requires iOS 16.0 or later"));
        assert!(html.contains("install-compatibility"));
        assert!(html.contains("install-info"));
        assert!(!html.contains("install-check"));
        assert!(!html.contains("此设备兼容"));

        let mut without_min_os = artifact();
        ios_metadata_mut(&mut without_min_os).minimum_os_version = None;
        let html = render(
            &without_min_os,
            "itms-services://example.test/install",
            None,
            Availability::Installable,
        );
        assert!(!html.contains("Requires iOS"));
        assert!(!html.contains(" or later"));
        assert!(!html.contains("此设备兼容"));
        assert!(!html.contains("compatible"));
    }

    #[test]
    fn render_is_english_only() {
        let html = render(
            &artifact(),
            "itms-services://example.test/install",
            None,
            Availability::Installable,
        );

        assert!(html.contains("<html lang=\"en\">"));
        assert!(!html.contains("zh-CN"));
        assert!(!html.contains("预览版"));
        assert!(!html.contains("版本 Version"));
        assert!(!html.contains("大小 Size"));
        assert!(!html.contains("安装 · Install"));
        assert!(!html.contains("此设备兼容"));
        assert!(html.contains(">Console</h1>"));
        assert!(html.contains("<p class=\"install-subtitle\">Preview Build</p>"));
        assert!(html.contains(">Version</dt>"));
        assert!(html.contains(">Size</dt>"));
        assert!(html.contains(">Bundle ID</dt>"));
        assert!(html.contains(">SHA-256</dt>"));
        assert!(html.contains(">Install</a>"));
    }

    #[test]
    fn version_label_prefers_short_version_and_deduplicates_equal_values() {
        let mut artifact = artifact();
        assert_eq!(version_label(&artifact), "2.9.0 (1142)");

        ios_metadata_mut(&mut artifact).bundle_short_version = Some("1142".into());
        assert_eq!(version_label(&artifact), "1142");

        ios_metadata_mut(&mut artifact).bundle_short_version = None;
        assert_eq!(version_label(&artifact), "1142");
    }

    #[test]
    fn android_page_uses_package_metadata_and_a_direct_apk_action() {
        let mut artifact = artifact();
        artifact.file_name = "Console.apk".into();
        artifact.platform_metadata = PlatformMetadata::Android(crate::model::AndroidMetadata {
            package_name: Some("com.rootstudio.console".into()),
            version_code: Some("1142".into()),
            version_name: Some("2.9.0".into()),
            min_sdk: Some(26),
            target_sdk: Some(36),
            certificate_sha256: Some("abcdef".into()),
        });

        let html = render(
            &artifact,
            "https://example.test/api/v1/artifacts/artifact-1/download.apk?download=grant",
            None,
            Availability::Installable,
        );

        assert!(html.contains("<title>Install Console · APK</title>"));
        assert!(html.contains(">Package name</dt>"));
        assert!(html.contains("com.rootstudio.console"));
        assert!(html.contains("Requires Android API 26 or later"));
        assert!(html.contains("2.9.0 (1142)"));
        assert!(html.contains("download.apk?download=grant"));
        assert!(html.contains("aria-label=\"Download Console for Android\""));
        assert!(html.contains(">Download APK</a>"));
        assert!(html.contains("allow installs from this browser"));
        assert!(!html.contains("itms-services://"));
        assert!(!html.contains("Requires iOS"));
    }

    #[test]
    fn android_page_is_honest_when_sdk_metadata_was_not_available() {
        let mut artifact = artifact();
        artifact.file_name = "Console.apk".into();
        artifact.platform_metadata = PlatformMetadata::Android(crate::model::AndroidMetadata {
            package_name: None,
            version_code: None,
            version_name: None,
            min_sdk: None,
            target_sdk: None,
            certificate_sha256: None,
        });

        let html = render(
            &artifact,
            "https://example.test/api/v1/artifacts/artifact-1/download.apk?download=grant",
            None,
            Availability::Installable,
        );

        assert!(html.contains(">Package name</dt>"));
        assert_eq!(html.matches(">Unavailable<").count(), 2);
        assert!(!html.contains("Requires Android API"));
        assert!(html.contains(">Download APK</a>"));
    }

    #[test]
    fn size_and_sha_are_human_readable_and_short_values_are_safe() {
        assert_eq!(abbreviate_sha256("abcdef"), "abcdef");
        assert_eq!(abbreviate_sha256("1234567890123"), "123456…890123");
        assert_eq!(
            abbreviate_sha256("前缀123456789012345后缀"),
            "前缀1234…2345后缀"
        );
    }

    #[test]
    fn render_contains_responsive_styles_and_install_cta() {
        let html = render(
            &artifact(),
            "itms-services://?action=download-manifest&url=https://example.test/manifest.plist",
            None,
            Availability::Installable,
        );

        assert!(html.contains("width: min(390px, 100%)"));
        assert!(html.contains(
            "<meta name=\"theme-color\" content=\"#ffffff\" media=\"(prefers-color-scheme: light)\">"
        ));
        assert!(html.contains(
            "<meta name=\"theme-color\" content=\"#18181b\" media=\"(prefers-color-scheme: dark)\">"
        ));
        assert!(html.contains("@media (prefers-color-scheme: dark)"));
        assert!(html.contains("--page-background: #f4f4f5"));
        assert!(html.contains("--surface: #ffffff"));
        assert!(html.contains("--surface-sunken: #f4f4f5"));
        assert!(html.contains("--surface-panel: rgba(255, 255, 255, 0.62)"));
        assert!(html.contains("--text-primary: #18181b"));
        assert!(html.contains("--border: rgba(24, 24, 27, 0.1)"));
        assert!(!html.contains("--success"));
        assert!(html.contains("--cta: #18181b"));
        assert!(html.contains("--cta-text: #ffffff"));
        assert!(html.contains("--page-background: #09090b"));
        assert!(html.contains("--surface: #18181b"));
        assert!(html.contains("--surface-sunken: #0e0e10"));
        assert!(html.contains("--text-primary: #fafafa"));
        assert!(html.contains("--border: rgba(255, 255, 255, 0.14)"));
        assert!(html.contains("--cta: #fafafa"));
        assert!(html.contains("--cta-text: #18181b"));
        assert!(html.contains("@media (max-width: 480px)"));
        let mobile_media_start = html.find("@media (max-width: 480px)").unwrap();
        let desktop_hero_start = html.find(".install-hero {").unwrap();
        assert!(
            html[desktop_hero_start..mobile_media_start]
                .contains("background: var(--surface-sunken);")
        );
        let mobile_hero_start =
            mobile_media_start + html[mobile_media_start..].find(".install-hero {").unwrap();
        assert!(html[mobile_hero_start..].contains("background: var(--surface);"));
        assert!(html.contains("padding: 0;"));
        assert!(html.contains("min-height: 100dvh;"));
        assert!(html.contains("border: 0;"));
        assert!(html.contains("border-radius: 0;"));
        assert!(html.contains("box-shadow: none;"));
        assert!(html.contains("html, body { background: var(--surface); }"));
        assert!(html.contains("background: var(--surface);"));
        assert!(html.contains("env(safe-area-inset-top, 0px)"));
        assert!(html.contains("env(safe-area-inset-right, 0px)"));
        assert!(html.contains("env(safe-area-inset-bottom, 0px)"));
        assert!(html.contains("env(safe-area-inset-left, 0px)"));
        assert!(html.contains("margin-top: auto;"));
        assert!(!html.contains("position: fixed"));
        assert!(!html.contains("position: sticky"));
        assert!(!html.contains("\n      height: 100dvh;"));
        assert!(html.contains("overflow-wrap: anywhere"));
        assert!(html.contains("@media (prefers-reduced-motion: reduce)"));
        assert!(html.contains(".install-cta:hover"));
        assert!(html.contains(".install-cta:active"));
        assert!(html.contains(".install-cta:focus-visible"));
        assert!(html.contains("<a class=\"install-cta\" href=\"itms-services://"));
        assert!(html.contains("class=\"install-info\""));
    }

    #[test]
    fn a_share_that_can_no_longer_install_still_explains_itself() {
        for (availability, heading) in [
            (Availability::LimitReached, "Download limit reached"),
            (Availability::Expired, "Link expired"),
        ] {
            let html = render(
                &artifact(),
                "itms-services://example.test/install",
                None,
                availability,
            );
            // The button is gone, but the page still identifies the build so
            // the reader can tell they are at the right link.
            assert!(!html.contains("class=\"install-cta\""), "{availability:?}");
            assert!(
                html.contains("class=\"install-notice\""),
                "{availability:?}"
            );
            assert!(html.contains(heading), "{availability:?}");
            assert!(html.contains(">Console</h1>"), "{availability:?}");
            assert!(html.contains("com.rootstudio.console"), "{availability:?}");
            assert!(!html.contains("itms-services://"), "{availability:?}");
        }
    }
}
