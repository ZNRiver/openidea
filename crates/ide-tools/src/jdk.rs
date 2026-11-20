//! JDK downloads (IDEA's Download JDK): Eclipse Temurin, Microsoft Build of OpenJDK,
//! Amazon Corretto, and Azul Zulu, unpacked into `<tools>/jdks/<vendor>-<version>` (or a
//! chosen location) where JDK discovery finds them, and automatically registered in user
//! environment variables (`JAVA_HOME`, `PATH`).

use crate::platform::{Arch, Os, Platform};
use crate::process::{Emit, Http};
use crate::sources::Download;
use crate::{InstallEvent, InstallGuard, Tools, archive, clear_quarantine, fetch_verified};
use ide_core::error::ErrorKind;
use ide_core::{CancellationToken, IdeError, IdeResult};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::path::{Path, PathBuf};

const ADOPTIUM_API: &str = "https://api.adoptium.net/v3";
const AZUL_API: &str = "https://api.azul.com/metadata/v1/zulu/packages/";

/// A JDK distribution the IDE can download.
#[derive(Clone, Debug, PartialEq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct JdkVendor {
    pub id: String,
    pub name: String,
}

pub const TEMURIN: &str = "temurin";
pub const MICROSOFT: &str = "microsoft";
pub const CORRETTO: &str = "corretto";
pub const ZULU: &str = "zulu";

pub fn vendors() -> Vec<JdkVendor> {
    vec![
        JdkVendor { id: TEMURIN.into(), name: "Eclipse Temurin".into() },
        JdkVendor { id: MICROSOFT.into(), name: "Microsoft Build of OpenJDK".into() },
        JdkVendor { id: CORRETTO.into(), name: "Amazon Corretto".into() },
        JdkVendor { id: ZULU.into(), name: "Azul Zulu".into() },
    ]
}

fn vendor_display_name(vendor: &str) -> &'static str {
    match vendor {
        TEMURIN => "Eclipse Temurin",
        MICROSOFT => "Microsoft Build of OpenJDK",
        CORRETTO => "Amazon Corretto",
        ZULU => "Azul Zulu",
        _ => "JDK",
    }
}

/// Java versions a vendor publishes.
#[derive(Clone, Debug, PartialEq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct JdkReleases {
    /// Feature versions, newest first.
    pub versions: Vec<u32>,
    /// The long-term-support ones among them.
    pub lts: Vec<u32>,
    /// The version offered first (the newest LTS).
    pub recommended: u32,
}

/// A JDK the IDE downloaded.
#[derive(Clone, Debug, PartialEq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DownloadedJdk {
    pub home: ide_core::IpcPath,
    /// Full version, e.g. `25.0.1`.
    pub version: String,
    pub major: u32,
}

/// Where downloaded JDKs go by default.
pub fn jdks_dir(app_data: &Path) -> PathBuf {
    crate::tools_dir(app_data).join("jdks")
}

fn proto(msg: impl Into<String>) -> IdeError {
    IdeError::new(ErrorKind::Protocol, msg)
}

fn vendor_or_err(vendor: &str) -> IdeResult<()> {
    match vendor {
        TEMURIN | MICROSOFT | CORRETTO | ZULU => Ok(()),
        _ => Err(IdeError::invalid(format!("unknown JDK vendor `{vendor}`"))),
    }
}

pub fn adoptium_releases_url() -> String {
    format!("{ADOPTIUM_API}/info/available_releases")
}

#[derive(Deserialize)]
struct AvailableReleases {
    available_releases: Vec<u32>,
    #[serde(default)]
    available_lts_releases: Vec<u32>,
    #[serde(default)]
    most_recent_lts: Option<u32>,
}

pub fn parse_releases(json: &str) -> IdeResult<JdkReleases> {
    let r: AvailableReleases =
        serde_json::from_str(json).map_err(|e| proto(format!("unexpected Adoptium response: {e}")))?;
    let mut versions = r.available_releases;
    versions.sort_unstable_by(|a, b| b.cmp(a));
    versions.dedup();
    let recommended = r
        .most_recent_lts
        .or_else(|| r.available_lts_releases.iter().copied().max())
        .or_else(|| versions.first().copied())
        .ok_or_else(|| proto("Adoptium lists no Java versions"))?;
    Ok(JdkReleases { versions, lts: r.available_lts_releases, recommended })
}

fn adoptium_os(os: Os) -> &'static str {
    match os {
        Os::Mac => "mac",
        Os::Linux => "linux",
        Os::Windows => "windows",
    }
}

fn adoptium_arch(arch: Arch) -> &'static str {
    match arch {
        Arch::X64 => "x64",
        Arch::Arm64 => "aarch64",
    }
}

/// The latest Temurin JDK build of Java `feature` for `platform`.
pub fn adoptium_assets_url(feature: u32, platform: Platform) -> String {
    let image = "jdk";
    format!(
        "{ADOPTIUM_API}/assets/latest/{feature}/hotspot?architecture={}&image_type={image}&os={}&vendor=eclipse",
        adoptium_arch(platform.arch),
        adoptium_os(platform.os),
    )
}

/// Direct evergreen URL for Microsoft Build of OpenJDK (LTS 11, 17, 21, 25).
pub fn microsoft_jdk_url(feature: u32, platform: Platform) -> String {
    let os = match platform.os {
        Os::Windows => "windows",
        Os::Mac => "macOS",
        Os::Linux => "linux",
    };
    let arch = match platform.arch {
        Arch::X64 => "x64",
        Arch::Arm64 => "aarch64",
    };
    let ext = if platform.os == Os::Windows { "zip" } else { "tar.gz" };
    format!("https://aka.ms/download-jdk/microsoft-jdk-{feature}-{os}-{arch}.{ext}")
}

/// Direct evergreen URL for Amazon Corretto (8, 11, 17, 21, 25).
pub fn corretto_jdk_url(feature: u32, platform: Platform) -> String {
    let os = match platform.os {
        Os::Windows => "windows",
        Os::Mac => "macos",
        Os::Linux => "linux",
    };
    // Amazon Corretto publishes Windows x64 builds only; ARM64 Windows runs them under emulation.
    let arch = match (platform.os, platform.arch) {
        (Os::Windows, _) => "x64",
        (_, Arch::X64) => "x64",
        (_, Arch::Arm64) => "aarch64",
    };
    let ext = if platform.os == Os::Windows { "zip" } else { "tar.gz" };
    format!("https://corretto.aws/downloads/latest/amazon-corretto-{feature}-{arch}-{os}-jdk.{ext}")
}

/// Azul Metadata API query for the latest Azul Zulu GA JDK build.
pub fn zulu_packages_url(feature: u32, platform: Platform) -> String {
    let os = match platform.os {
        Os::Windows => "windows",
        Os::Mac => "macos",
        Os::Linux => "linux",
    };
    let arch = match platform.arch {
        Arch::X64 => "x64",
        Arch::Arm64 => "arm64",
    };
    let ext = if platform.os == Os::Windows { "zip" } else { "tar.gz" };
    format!(
        "{AZUL_API}?java_version={feature}&os={os}&arch={arch}&archive_type={ext}&java_package_type=jdk&javafx_bundled=false&latest=true&release_status=ga&availability_types=CA"
    )
}

#[derive(Deserialize)]
struct ZuluPackage {
    name: String,
    download_url: String,
    #[serde(default)]
    java_version: Vec<u32>,
    #[serde(default)]
    sha256_hash: Option<String>,
}

pub fn zulu_pick(json: &str, feature: u32) -> IdeResult<Option<(Download, u32)>> {
    let pkgs: Vec<ZuluPackage> =
        serde_json::from_str(json).map_err(|e| proto(format!("unexpected Azul Zulu response: {e}")))?;
    let Some(pkg) = pkgs.into_iter().next() else {
        return Ok(None);
    };
    let major = pkg.java_version.first().copied().unwrap_or(feature);
    let version = if pkg.java_version.is_empty() {
        feature.to_string()
    } else {
        pkg.java_version.iter().take(3).map(u32::to_string).collect::<Vec<_>>().join(".")
    };
    let sha256 = pkg.sha256_hash.map(|c| c.trim().to_ascii_lowercase()).filter(|c| c.len() == 64);
    Ok(Some((
        Download { version, name: pkg.name, url: pkg.download_url, size: None, sha256, checksum_url: None },
        major,
    )))
}

fn version_from_filename(name: &str, feature: u32) -> String {
    let prefix = format!("{feature}.");
    if let Some(idx) = name.find(&prefix) {
        let rest = &name[idx..];
        let token: String =
            rest.chars().take_while(|c| c.is_ascii_digit() || matches!(c, '.' | '_')).collect();
        let trimmed = token.trim_end_matches(['.', '_']);
        if trimmed.len() > prefix.len() {
            return trimmed.to_owned();
        }
    }
    feature.to_string()
}

fn filename_from_url(url: &str, fallback: &str) -> String {
    let clean = url.split(['?', '#']).next().unwrap_or(url);
    clean.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(fallback).to_owned()
}

#[derive(Deserialize)]
struct Asset {
    binary: Binary,
    version: AssetVersion,
}

#[derive(Deserialize)]
struct Binary {
    #[serde(default)]
    image_type: Option<String>,
    #[serde(default)]
    heap_size: Option<String>,
    #[serde(default)]
    os: Option<String>,
    package: Package,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    link: String,
    #[serde(default)]
    checksum: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Deserialize)]
struct AssetVersion {
    major: u32,
    #[serde(default)]
    minor: u32,
    #[serde(default)]
    security: u32,
    #[serde(default)]
    openjdk_version: Option<String>,
}

impl AssetVersion {
    /// `25.0.1` (`openjdk_version` without its build; `1.8.0_472` style for Java 8).
    fn display(&self) -> String {
        let plain = || format!("{}.{}.{}", self.major, self.minor, self.security);
        match &self.openjdk_version {
            Some(v) => {
                let v = v.split(['+', '-']).next().unwrap_or(v);
                if v.is_empty() { plain() } else { v.to_owned() }
            }
            None => plain(),
        }
    }
}

/// The JDK archive (`.tar.gz` on macOS / Linux, `.zip` on Windows) in an Adoptium
/// `assets/latest` response; `None` when there is no build for the platform.
pub fn adoptium_pick(json: &str, platform: Platform) -> IdeResult<Option<(Download, u32)>> {
    let assets: Vec<Asset> =
        serde_json::from_str(json).map_err(|e| proto(format!("unexpected Adoptium response: {e}")))?;
    let ext = if platform.os == Os::Windows { ".zip" } else { ".tar.gz" };
    let os = adoptium_os(platform.os);
    let found = assets.into_iter().find(|a| {
        a.binary.image_type.as_deref().is_none_or(|t| t == "jdk")
            && a.binary.heap_size.as_deref().is_none_or(|h| h == "normal")
            && a.binary.os.as_deref().is_none_or(|o| o == os)
            && a.binary.package.name.ends_with(ext)
    });
    Ok(found.map(|a| {
        let sha256 = a.binary.package.checksum.map(|c| c.trim().to_ascii_lowercase()).filter(|c| c.len() == 64);
        (
            Download {
                version: a.version.display(),
                name: a.binary.package.name,
                url: a.binary.package.link,
                size: a.binary.package.size,
                sha256,
                checksum_url: None,
            },
            a.version.major,
        )
    }))
}

/// Folder name of a downloaded JDK, as IDEA names them: `temurin-25.0.1`.
pub fn jdk_dir_name(vendor: &str, version: &str) -> String {
    let safe: String =
        version.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_') { c } else { '-' }).collect();
    format!("{vendor}-{safe}")
}

fn is_jdk_home(dir: &Path, platform: Platform) -> bool {
    dir.join("release").is_file() && dir.join("bin").join(platform.exe("java")).is_file()
}

/// The JDK home inside an unpacked JDK: `dir` itself, `dir/Contents/Home`
/// (a macOS bundle), or either of those below one top-level folder
/// (`jdk-25.0.1+8/`, `jdk-25.0.1+8/Contents/Home`).
pub fn find_jdk_home(dir: &Path, platform: Platform) -> Option<PathBuf> {
    let candidates = |d: &Path| [d.to_path_buf(), d.join("Contents").join("Home")];
    if let Some(home) = candidates(dir).into_iter().find(|h| is_jdk_home(h, platform)) {
        return Some(home);
    }
    let mut subdirs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    subdirs.sort();
    subdirs.iter().flat_map(|d| candidates(d)).find(|h| is_jdk_home(h, platform))
}

fn usable_java_home(dir: &Path, platform: Platform, runtime: bool) -> Option<PathBuf> {
    find_jdk_home(dir, platform).filter(|home| !runtime || ide_core::java_runtime::supports_java_bundles(home))
}

/// The directory to move into place: the archive's single top-level folder
/// holding the JDK, else everything that was unpacked.
fn unpacked_root(pkg: &Path, home: &Path) -> PathBuf {
    match home.strip_prefix(pkg).ok().and_then(|rel| rel.components().next()) {
        Some(first) if first.as_os_str() != "Contents" => pkg.join(first),
        _ => pkg.to_path_buf(),
    }
}

impl Tools {
    fn http(&self) -> IdeResult<Http> {
        Http::new(&self.system_path)
    }

    /// Java versions `vendor` publishes.
    pub fn jdk_releases(&self, vendor: &str, cancel: &CancellationToken) -> IdeResult<JdkReleases> {
        vendor_or_err(vendor)?;
        match vendor {
            TEMURIN => parse_releases(&self.http()?.get_text(&adoptium_releases_url(), cancel)?),
            MICROSOFT => Ok(JdkReleases { versions: vec![25, 21, 17, 11], lts: vec![25, 21, 17, 11], recommended: 21 }),
            CORRETTO => {
                Ok(JdkReleases { versions: vec![25, 21, 17, 11, 8], lts: vec![25, 21, 17, 11, 8], recommended: 21 })
            }
            ZULU => Ok(JdkReleases { versions: vec![25, 21, 17, 11, 8], lts: vec![25, 21, 17, 11, 8], recommended: 21 }),
            _ => unreachable!("validated above"),
        }
    }

    /// Downloads the latest `vendor` JDK of Java `feature` into
    /// `<location>/<vendor>-<version>` (`location` defaults to the tools' `jdks`
    /// folder), verifying its SHA-256 when provided, and registers `JAVA_HOME` + `PATH`.
    /// A JDK already there is reused.
    pub fn download_jdk(
        &self,
        vendor: &str,
        feature: u32,
        location: Option<&Path>,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedJdk> {
        self.download_java(vendor, feature, location, false, cancel, emit)
    }

    /// Downloads a full JDK for a language server and its bundles (java-debug
    /// needs `jdk.jdi`). Kept outside project JDK discovery roots.
    pub fn download_java_runtime(
        &self,
        feature: u32,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedJdk> {
        self.download_java(TEMURIN, feature, Some(location), true, cancel, emit)
    }

    fn resolve_jdk_download(
        &self,
        http: &Http,
        vendor: &str,
        feature: u32,
        cancel: &CancellationToken,
    ) -> IdeResult<Option<(Download, u32)>> {
        match vendor {
            TEMURIN => {
                let mut picked =
                    adoptium_pick(&http.get_text(&adoptium_assets_url(feature, self.platform), cancel)?, self.platform)?;
                if picked.is_none()
                    && matches!(self.platform.os, Os::Mac | Os::Windows)
                    && self.platform.arch == Arch::Arm64
                {
                    let intel = Platform { os: self.platform.os, arch: Arch::X64 };
                    picked = adoptium_pick(&http.get_text(&adoptium_assets_url(feature, intel), cancel)?, intel)?;
                }
                Ok(picked)
            }
            MICROSOFT => {
                let try_url = |platform: Platform| -> IdeResult<Option<(Download, u32)>> {
                    let url = microsoft_jdk_url(feature, platform);
                    let fallback_name = filename_from_url(&url, "microsoft-jdk.zip");
                    match http.effective_url(&url, cancel) {
                        Ok(final_url) if !final_url.is_empty() => {
                            let name = filename_from_url(&final_url, &fallback_name);
                            let version = version_from_filename(&name, feature);
                            Ok(Some((
                                Download { version, name, url: final_url, size: None, sha256: None, checksum_url: None },
                                feature,
                            )))
                        }
                        Err(e) if e.kind == ErrorKind::Cancelled => Err(e),
                        _ => Ok(None),
                    }
                };
                let mut picked = try_url(self.platform)?;
                if picked.is_none()
                    && matches!(self.platform.os, Os::Mac | Os::Windows)
                    && self.platform.arch == Arch::Arm64
                {
                    picked = try_url(Platform { os: self.platform.os, arch: Arch::X64 })?;
                }
                Ok(picked)
            }
            CORRETTO => {
                let url = corretto_jdk_url(feature, self.platform);
                let fallback_name = filename_from_url(&url, "amazon-corretto.zip");
                let final_url = match http.effective_url(&url, cancel) {
                    Ok(u) if !u.is_empty() => u,
                    Err(e) if e.kind == ErrorKind::Cancelled => return Err(e),
                    _ => url,
                };
                let name = filename_from_url(&final_url, &fallback_name);
                let version = version_from_filename(&name, feature);
                Ok(Some((
                    Download { version, name, url: final_url, size: None, sha256: None, checksum_url: None },
                    feature,
                )))
            }
            ZULU => {
                let mut picked =
                    zulu_pick(&http.get_text(&zulu_packages_url(feature, self.platform), cancel)?, feature)?;
                if picked.is_none()
                    && matches!(self.platform.os, Os::Mac | Os::Windows)
                    && self.platform.arch == Arch::Arm64
                {
                    let intel = Platform { os: self.platform.os, arch: Arch::X64 };
                    picked = zulu_pick(&http.get_text(&zulu_packages_url(feature, intel), cancel)?, feature)?;
                }
                Ok(picked)
            }
            _ => Err(IdeError::invalid(format!("unknown JDK vendor `{vendor}`"))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn download_java(
        &self,
        vendor: &str,
        feature: u32,
        location: Option<&Path>,
        runtime: bool,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedJdk> {
        vendor_or_err(vendor)?;
        let _guard = InstallGuard::acquire(&format!("jdk-{vendor}-{feature}"))?;
        let http = self.http()?;
        let display = vendor_display_name(vendor);
        let location = location.map(Path::to_path_buf).unwrap_or_else(|| self.root.join("jdks"));
        emit(InstallEvent::Line { text: format!("Resolving the latest {display} {feature}…") });
        let (dl, major) = self.resolve_jdk_download(&http, vendor, feature, cancel)?.ok_or_else(|| {
            IdeError::unavailable(format!(
                "{display} {feature} has no build for {:?} {:?}",
                self.platform.os, self.platform.arch
            ))
        })?;
        let target = location.join(jdk_dir_name(vendor, &dl.version));
        let done = |home: PathBuf| DownloadedJdk { home: home.into(), version: dl.version.clone(), major };
        let home = if let Some(existing) = usable_java_home(&target, self.platform, runtime) {
            emit(InstallEvent::Line { text: format!("{} is already installed", target.display()) });
            existing
        } else {
            emit(InstallEvent::Line { text: format!("Downloading {} ({})", dl.name, dl.url) });
            std::fs::create_dir_all(&location)?;
            // Next to the target, so moving it into place never crosses file systems.
            let staging = location.join(format!(".staging-{}", jdk_dir_name(vendor, &dl.version)));
            let _ = std::fs::remove_dir_all(&staging);
            std::fs::create_dir_all(&staging)?;
            let result = (|| {
                let archive = fetch_verified(&self.downloads, &http, &dl, &staging, cancel, emit)?;
                emit(InstallEvent::Line { text: "Extracting…".into() });
                let pkg = staging.join("pkg");
                archive::extract(&archive, archive::ArchiveKind::from_name(&dl.name), &pkg, "java", cancel)?;
                let home = usable_java_home(&pkg, self.platform, runtime)
                    .ok_or_else(|| IdeError::new(ErrorKind::Io, format!("no JDK found in {}", dl.name)))?;
                // Last point to stop: from here the JDK replaces what is at `target`.
                if cancel.is_cancelled() {
                    return Err(IdeError::cancelled());
                }
                if target.exists() {
                    std::fs::remove_dir_all(&target)?;
                }
                std::fs::rename(unpacked_root(&pkg, &home), &target)?;
                clear_quarantine(&target);
                usable_java_home(&target, self.platform, runtime)
                    .ok_or_else(|| IdeError::new(ErrorKind::Io, format!("no JDK found in {}", target.display())))
            })();
            let _ = std::fs::remove_dir_all(&staging);
            let home = result?;
            emit(InstallEvent::Line { text: format!("{display} {} installed in {}", dl.version, home.display()) });
            home
        };
        if !runtime {
            let home_s = home.to_string_lossy().into_owned();
            match ide_core::process::register_user_env(&[home.join("bin")], &[("JAVA_HOME", &home_s)]) {
                Ok(()) => emit(InstallEvent::Line {
                    text: "Added JDK to environment variables (JAVA_HOME, PATH).".into(),
                }),
                Err(err) => emit(InstallEvent::Line {
                    text: format!("Warning: could not update user environment variables: {err}"),
                }),
            }
        }
        Ok(done(home))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC_ARM: Platform = Platform { os: Os::Mac, arch: Arch::Arm64 };
    const LINUX_X64: Platform = Platform { os: Os::Linux, arch: Arch::X64 };
    const WIN_X64: Platform = Platform { os: Os::Windows, arch: Arch::X64 };

    #[test]
    fn releases() {
        let json = r#"{"available_lts_releases":[8,11,17,21,25],"available_releases":[8,11,16,17,21,24,25,26],
            "most_recent_feature_release":26,"most_recent_lts":25,"tip_version":27}"#;
        let r = parse_releases(json).unwrap();
        assert_eq!(r.versions, [26, 25, 24, 21, 17, 16, 11, 8]);
        assert_eq!(r.lts, [8, 11, 17, 21, 25]);
        assert_eq!(r.recommended, 25);
        assert!(parse_releases("{}").is_err());
    }

    #[test]
    fn asset_urls() {
        assert_eq!(
            adoptium_assets_url(25, MAC_ARM),
            "https://api.adoptium.net/v3/assets/latest/25/hotspot?architecture=aarch64&image_type=jdk&os=mac&vendor=eclipse"
        );
        assert_eq!(
            adoptium_assets_url(21, WIN_X64),
            "https://api.adoptium.net/v3/assets/latest/21/hotspot?architecture=x64&image_type=jdk&os=windows&vendor=eclipse"
        );
        assert!(adoptium_assets_url(17, LINUX_X64).contains("os=linux"));
        assert_eq!(
            microsoft_jdk_url(21, WIN_X64),
            "https://aka.ms/download-jdk/microsoft-jdk-21-windows-x64.zip"
        );
        assert_eq!(
            corretto_jdk_url(21, WIN_X64),
            "https://corretto.aws/downloads/latest/amazon-corretto-21-x64-windows-jdk.zip"
        );
        assert!(zulu_packages_url(21, WIN_X64).contains("java_version=21&os=windows&arch=x64&archive_type=zip"));
    }

    fn asset(name: &str, os: &str, heap: &str, checksum: &str) -> String {
        format!(
            r#"{{"binary":{{"architecture":"x64","heap_size":"{heap}","image_type":"jdk","os":"{os}",
                "installer":{{"name":"x.msi","link":"https://e/x.msi","checksum":"00"}},
                "package":{{"name":"{name}","link":"https://e/{name}","checksum":"{checksum}","size":1234}}}},
              "release_name":"jdk-25.0.1+8","vendor":"eclipse",
              "version":{{"major":25,"minor":0,"security":1,"build":8,"openjdk_version":"25.0.1+8-LTS"}}}}"#
        )
    }

    #[test]
    fn picks_the_platform_archive() {
        let sum = "36DC14D797AAFF6D89AAB302CD053FDBA6207DFDCF772052FA7B023A94B0E857";
        let json = format!(
            "[{},{},{}]",
            asset("OpenJDK25U-jdk_x64_windows_hotspot_25.0.1_8.zip", "windows", "large", sum),
            asset("OpenJDK25U-jdk_x64_windows_hotspot_25.0.1_8.zip", "windows", "normal", sum),
            asset("OpenJDK25U-jdk_x64_linux_hotspot_25.0.1_8.tar.gz", "linux", "normal", sum),
        );
        let (dl, major) = adoptium_pick(&json, WIN_X64).unwrap().unwrap();
        assert_eq!(dl.name, "OpenJDK25U-jdk_x64_windows_hotspot_25.0.1_8.zip");
        assert_eq!(dl.url, "https://e/OpenJDK25U-jdk_x64_windows_hotspot_25.0.1_8.zip");
        assert_eq!(dl.sha256.as_deref(), Some(sum.to_ascii_lowercase().as_str()));
        assert_eq!((dl.version.as_str(), major, dl.size), ("25.0.1", 25, Some(1234)));
        let (dl, _) = adoptium_pick(&json, LINUX_X64).unwrap().unwrap();
        assert!(dl.name.ends_with(".tar.gz"));
        // No build for the platform (Java 8 on Apple Silicon): an empty list.
        assert_eq!(adoptium_pick("[]", MAC_ARM).unwrap(), None);
        assert_eq!(adoptium_pick(&json, MAC_ARM).unwrap(), None);
        assert!(adoptium_pick("{\"errorMessage\":\"x\"}", MAC_ARM).is_err());
    }

    #[test]
    fn runtime_assets_require_jdk_images() {
        for platform in [MAC_ARM, LINUX_X64, WIN_X64] {
            assert!(adoptium_assets_url(25, platform).contains("image_type=jdk"));
            let ext = if platform.os == Os::Windows { "zip" } else { "tar.gz" };
            let jdk = asset(&format!("java.{ext}"), adoptium_os(platform.os), "normal", "");
            let jre = jdk.replace("\"image_type\":\"jdk\"", "\"image_type\":\"jre\"");
            assert!(adoptium_pick(&format!("[{jre},{jdk}]"), platform).unwrap().is_some());
            assert!(adoptium_pick(&format!("[{jre}]"), platform).unwrap().is_none());
        }
    }

    #[test]
    fn stale_runtime_is_replaced_even_at_the_same_version() {
        for platform in [MAC_ARM, LINUX_X64, WIN_X64] {
            let tmp = tempfile::tempdir().unwrap();
            let target = tmp.path().join(jdk_dir_name(TEMURIN, "25.0.1"));
            let home = if platform.os == Os::Mac { target.join("Contents/Home") } else { target.clone() };
            fake_jdk(&home, platform);
            let javac = home.join("bin").join(if cfg!(windows) { "javac.exe" } else { "javac" });
            std::fs::write(&javac, "").unwrap();
            for modules in [None, Some("java.base java.compiler jdk.jdix"), Some("java.base jdk.jdi.agent")] {
                let line = modules.map(|m| format!("MODULES=\"{m}\"\n")).unwrap_or_default();
                std::fs::write(home.join("release"), format!("JAVA_VERSION=\"25.0.1\"\n{line}")).unwrap();
                assert!(usable_java_home(&target, platform, true).is_none(), "{modules:?}");
                assert_eq!(usable_java_home(&target, platform, false), Some(home.clone()));
            }
            std::fs::write(
                home.join("release"),
                "JAVA_VERSION=\"25.0.1\"\nMODULES=\"java.base jdk.compiler jdk.jdi jdk.unsupported\"\n",
            )
            .unwrap();
            assert_eq!(usable_java_home(&target, platform, true), Some(home.clone()));
            std::fs::write(home.join("release"), "JAVA_VERSION=\"25.0.1\"\n").unwrap();
            std::fs::create_dir_all(home.join("jmods")).unwrap();
            for module in ["jdk.compiler", "jdk.jdi", "jdk.unsupported"] {
                std::fs::write(home.join("jmods").join(format!("{module}.jmod")), "").unwrap();
            }
            assert_eq!(usable_java_home(&target, platform, true), Some(home.clone()));
            std::fs::write(
                home.join("release"),
                "IMAGE_TYPE=\"JRE\"\nMODULES=\"java.base jdk.compiler jdk.jdi jdk.unsupported\"\n",
            )
            .unwrap();
            assert!(usable_java_home(&target, platform, true).is_none(), "Temurin JRE release can list absent modules");
            std::fs::write(home.join("release"), "MODULES=\"java.base jdk.compiler jdk.jdi jdk.unsupported\"\n")
                .unwrap();
            std::fs::remove_file(javac).unwrap();
            assert!(usable_java_home(&target, platform, true).is_none(), "custom runtime without compiler");
            std::fs::write(home.join("release"), "MODULES=\"java.base\"\n").unwrap();
            assert!(usable_java_home(&target, platform, true).is_none(), "release describes the runtime image");
        }
    }

    #[test]
    fn java_8_versions_and_dir_names() {
        let v = AssetVersion { major: 8, minor: 0, security: 472, openjdk_version: Some("1.8.0_472-b08".into()) };
        assert_eq!(v.display(), "1.8.0_472");
        let v = AssetVersion { major: 21, minor: 0, security: 9, openjdk_version: None };
        assert_eq!(v.display(), "21.0.9");
        assert_eq!(jdk_dir_name(TEMURIN, "25.0.1"), "temurin-25.0.1");
        assert_eq!(jdk_dir_name(MICROSOFT, "21.0.6"), "microsoft-21.0.6");
        assert_eq!(jdk_dir_name(TEMURIN, "1.8.0_472"), "temurin-1.8.0_472");
    }

    fn fake_jdk(home: &Path, platform: Platform) {
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::write(home.join("release"), "JAVA_VERSION=\"25.0.1\"\n").unwrap();
        std::fs::write(home.join("bin").join(platform.exe("java")), "").unwrap();
    }

    #[test]
    fn finds_the_home_of_each_archive_layout() {
        let tmp = tempfile::tempdir().unwrap();
        // macOS: jdk-25.0.1+8/Contents/Home
        let mac = tmp.path().join("mac");
        fake_jdk(&mac.join("jdk-25.0.1+8/Contents/Home"), MAC_ARM);
        std::fs::create_dir_all(mac.join("jdk-25.0.1+8/Contents/MacOS")).unwrap();
        let home = find_jdk_home(&mac, MAC_ARM).unwrap();
        assert_eq!(home, mac.join("jdk-25.0.1+8/Contents/Home"));
        assert_eq!(unpacked_root(&mac, &home), mac.join("jdk-25.0.1+8"));
        // Once moved into place, the bundle itself.
        assert_eq!(find_jdk_home(&mac.join("jdk-25.0.1+8"), MAC_ARM).unwrap(), mac.join("jdk-25.0.1+8/Contents/Home"));
        // Linux tar.gz / Windows zip: jdk-25.0.1+8/bin/java[.exe]
        for platform in [LINUX_X64, WIN_X64] {
            let dir = tmp.path().join(format!("{:?}", platform.os));
            fake_jdk(&dir.join("jdk-25.0.1+8"), platform);
            let home = find_jdk_home(&dir, platform).unwrap();
            assert_eq!(home, dir.join("jdk-25.0.1+8"));
            assert_eq!(unpacked_root(&dir, &home), dir.join("jdk-25.0.1+8"));
        }
        // Without a top-level folder, everything unpacked is the JDK.
        let flat = tmp.path().join("flat");
        fake_jdk(&flat, LINUX_X64);
        assert_eq!(find_jdk_home(&flat, LINUX_X64).unwrap(), flat);
        assert_eq!(unpacked_root(&flat, &flat), flat);
        let bundle = tmp.path().join("bundle");
        fake_jdk(&bundle.join("Contents/Home"), MAC_ARM);
        assert_eq!(unpacked_root(&bundle, &bundle.join("Contents/Home")), bundle);
        // A Windows layout isn't a JDK for Linux (no `bin/java`), nor is a folder without `release`.
        assert_eq!(find_jdk_home(&tmp.path().join("Windows"), LINUX_X64), None);
        std::fs::create_dir_all(tmp.path().join("norelease/bin")).unwrap();
        std::fs::write(tmp.path().join("norelease/bin/java"), "").unwrap();
        assert_eq!(find_jdk_home(&tmp.path().join("norelease"), LINUX_X64), None);
    }

    #[test]
    fn unknown_vendors_are_refused() {
        let tools = Tools::with_paths(PathBuf::from("/nonexistent"), Default::default(), LINUX_X64);
        let err = tools.download_jdk("acme", 25, None, &CancellationToken::new(), &|_| {}).unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidInput, "{err:?}");
    }
}
