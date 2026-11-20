//! Language SDK, runtime, and build-tool downloads across all supported ecosystems:
//! Python (`uv` + Python), Node.js (LTS / Current, `pnpm`, `bun`, `deno`), Rust (`rustup`),
//! Go, Zig (Stable / Master), .NET (LTS / STS), PHP + Composer, Dart, Flutter, Maven, Gradle,
//! Ruby (Windows), and Haskell (`ghcup`).
//!
//! Downloaded toolchains are unpacked into `<tools>/sdks/<id>-<version>` (or a chosen
//! location) with an `openidea-sdk.json` manifest and automatically registered in both the
//! running IDE's environment overlay and the user's persistent OS environment variables.

use crate::platform::{Arch, AssetRule, Os, Platform, rule};
use crate::process::{Emit, Http, run_streamed};
use crate::registry::ReleaseSource;
use crate::sources::Download;
use crate::{InstallEvent, InstallGuard, Tools, archive, clear_quarantine, fetch_verified};
use ide_core::error::ErrorKind;
use ide_core::{CancellationToken, IdeError, IdeResult};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SDK_MANIFEST: &str = "openidea-sdk.json";

/// A downloadable runtime, SDK, or toolchain distribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct ToolchainProvider {
    pub id: String,
    /// Logical language/ecosystem key (`python`, `node`, `rust`, `go`, `zig`, `dotnet`, `php`, `dart`, `flutter`, `java-build`, `ruby`, `haskell`).
    pub runtime: String,
    pub name: String,
    pub description: String,
    /// Primary executables provided by this distribution (e.g. `["uv", "python"]`, `["node", "npm"]`).
    pub programs: Vec<String>,
    pub recommended: bool,
    pub requires_admin: bool,
}

/// Result of downloading and registering a language toolchain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DownloadedToolchain {
    pub id: String,
    pub runtime: String,
    pub name: String,
    pub version: String,
    pub home: ide_core::IpcPath,
    pub bin_dir: ide_core::IpcPath,
    pub env_vars: Vec<(String, String)>,
}

/// Manifest stored in `<tools>/sdks/<dir>/openidea-sdk.json` so portable installs
/// automatically recover their `PATH` directories at startup.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SdkManifest {
    id: String,
    runtime: String,
    name: String,
    version: String,
    /// Relative path from the SDK directory to its `bin` folder (`"."` when executables live at the root).
    bin_dir: String,
    #[serde(default)]
    env_vars: Vec<(String, String)>,
}

/// Default location for downloaded language SDKs and runtimes (`<app_data>/tools/sdks`).
pub fn sdks_dir(app_data: &Path) -> PathBuf {
    crate::tools_dir(app_data).join("sdks")
}

/// Discovers `bin` directories of every SDK previously unpacked into `<tools>/sdks/` so
/// portable installations keep them on `PATH` across sessions and machines.
pub fn installed_sdk_bin_dirs(tools_root: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let sdks = tools_root.join("sdks");
    let Ok(entries) = std::fs::read_dir(&sdks) else {
        return dirs;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || path.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(path.join(SDK_MANIFEST))
            && let Ok(manifest) = serde_json::from_slice::<SdkManifest>(&bytes)
        {
            let bin = if manifest.bin_dir == "." || manifest.bin_dir.is_empty() {
                path.clone()
            } else {
                path.join(&manifest.bin_dir)
            };
            if bin.is_dir() && !dirs.contains(&bin) {
                dirs.push(bin);
            }
            continue;
        }
        let bin = path.join("bin");
        if bin.is_dir() {
            dirs.push(bin);
        } else {
            dirs.push(path);
        }
    }
    dirs
}

/// Providers offered on the current machine, optionally filtered by `runtime`.
pub fn providers(runtime: Option<&str>) -> Vec<ToolchainProvider> {
    providers_for(Platform::current(), runtime)
}

/// Providers offered on `platform` (pure for testing).
pub fn providers_for(platform: Platform, runtime: Option<&str>) -> Vec<ToolchainProvider> {
    let mut all = vec![
        ToolchainProvider {
            id: "uv".into(),
            runtime: "python".into(),
            name: "uv + Python (Recommended)".into(),
            description: "Downloads Astral's fast Python package & project manager (`uv`) and installs a managed Python interpreter automatically.".into(),
            programs: vec!["uv".into(), "python".into(), "python3".into()],
            recommended: true,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "node-lts".into(),
            runtime: "node".into(),
            name: "Node.js LTS (with npm & npx)".into(),
            description: "Official Long-Term Support Node.js distribution including npm, npx, and Corepack.".into(),
            programs: vec!["node".into(), "npm".into(), "npx".into()],
            recommended: true,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "node-current".into(),
            runtime: "node".into(),
            name: "Node.js Current (Latest)".into(),
            description: "Latest official Node.js release with the newest JavaScript/TypeScript runtime features.".into(),
            programs: vec!["node".into(), "npm".into(), "npx".into()],
            recommended: false,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "pnpm".into(),
            runtime: "node".into(),
            name: "pnpm (Standalone)".into(),
            description: "Fast, disk-space efficient standalone pnpm package manager binary.".into(),
            programs: vec!["pnpm".into()],
            recommended: false,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "bun".into(),
            runtime: "node".into(),
            name: "Bun".into(),
            description: "All-in-one JavaScript & TypeScript runtime, bundler, test runner, and package manager.".into(),
            programs: vec!["bun".into(), "bunx".into()],
            recommended: false,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "deno".into(),
            runtime: "node".into(),
            name: "Deno".into(),
            description: "Secure JavaScript and TypeScript runtime with built-in tooling.".into(),
            programs: vec!["deno".into()],
            recommended: false,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "rustup-stable".into(),
            runtime: "rust".into(),
            name: "Rust (rustup — Stable)".into(),
            description: "Official Rust toolchain installer (rustc, cargo, rustup, clippy, rustfmt) on the stable channel.".into(),
            programs: vec!["cargo".into(), "rustc".into(), "rustup".into()],
            recommended: true,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "rustup-nightly".into(),
            runtime: "rust".into(),
            name: "Rust (rustup — Nightly)".into(),
            description: "Official Rust toolchain installer on the nightly channel.".into(),
            programs: vec!["cargo".into(), "rustc".into(), "rustup".into()],
            recommended: false,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "go".into(),
            runtime: "go".into(),
            name: "Go SDK (Official)".into(),
            description: "Official Go compiler, standard library, and `go` tool from go.dev.".into(),
            programs: vec!["go".into(), "gofmt".into()],
            recommended: true,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "zig".into(),
            runtime: "zig".into(),
            name: "Zig (Latest Stable)".into(),
            description: "Official standalone Zig compiler and build system from ziglang.org.".into(),
            programs: vec!["zig".into()],
            recommended: true,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "zig-master".into(),
            runtime: "zig".into(),
            name: "Zig (Master / Dev)".into(),
            description: "Latest Zig development build from ziglang.org.".into(),
            programs: vec!["zig".into()],
            recommended: false,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "dotnet-lts".into(),
            runtime: "dotnet".into(),
            name: ".NET SDK (LTS)".into(),
            description: "Official Microsoft .NET Long-Term Support SDK for C# and F# (portable user install, no admin required).".into(),
            programs: vec!["dotnet".into()],
            recommended: true,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "dotnet-sts".into(),
            runtime: "dotnet".into(),
            name: ".NET SDK (Latest / STS)".into(),
            description: "Latest Microsoft .NET Standard-Term Support SDK for C# and F#.".into(),
            programs: vec!["dotnet".into()],
            recommended: false,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "php".into(),
            runtime: "php".into(),
            name: "PHP + Composer".into(),
            description: "Portable PHP CLI preconfigured with common extensions plus Composer package manager.".into(),
            programs: vec!["php".into(), "composer".into()],
            recommended: true,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "dart".into(),
            runtime: "dart".into(),
            name: "Dart SDK (Official)".into(),
            description: "Official Dart SDK archive from Google.".into(),
            programs: vec!["dart".into()],
            recommended: true,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "flutter".into(),
            runtime: "flutter".into(),
            name: "Flutter SDK (includes Dart)".into(),
            description: "Official stable Flutter SDK archive (also includes the bundled Dart SDK).".into(),
            programs: vec!["flutter".into(), "dart".into()],
            recommended: true,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "maven".into(),
            runtime: "java-build".into(),
            name: "Apache Maven (Latest)".into(),
            description: "Portable Apache Maven build tool for Java and Kotlin projects.".into(),
            programs: vec!["mvn".into()],
            recommended: true,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "gradle".into(),
            runtime: "java-build".into(),
            name: "Gradle (Latest)".into(),
            description: "Portable Gradle build tool for Java and Kotlin projects.".into(),
            programs: vec!["gradle".into()],
            recommended: false,
            requires_admin: false,
        },
        ToolchainProvider {
            id: "ghcup".into(),
            runtime: "haskell".into(),
            name: "Haskell (GHCup — GHC + Cabal)".into(),
            description: "Official GHCup toolchain installer with GHC and Cabal.".into(),
            programs: vec!["cabal".into(), "ghc".into(), "ghcup".into()],
            recommended: true,
            requires_admin: false,
        },
    ];
    if platform.os == Os::Windows {
        all.push(ToolchainProvider {
            id: "ruby-installer".into(),
            runtime: "ruby".into(),
            name: "Ruby + DevKit (RubyInstaller2)".into(),
            description: "Official RubyInstaller2 with MSYS2 DevKit (installs per-user without admin).".into(),
            programs: vec!["ruby".into(), "gem".into(), "bundle".into()],
            recommended: true,
            requires_admin: false,
        });
    }
    if let Some(rt) = runtime.map(str::trim).filter(|s| !s.is_empty()) {
        all.retain(|p| p.runtime == rt || p.programs.iter().any(|prog| prog == rt));
    }
    all
}

const UV_ASSETS: &[AssetRule] = &[
    rule(Os::Windows, Arch::X64, "uv-x86_64-pc-windows-msvc.zip"),
    rule(Os::Windows, Arch::Arm64, "uv-aarch64-pc-windows-msvc.zip"),
    rule(Os::Linux, Arch::X64, "uv-x86_64-unknown-linux-gnu.tar.gz"),
    rule(Os::Linux, Arch::Arm64, "uv-aarch64-unknown-linux-gnu.tar.gz"),
    rule(Os::Mac, Arch::Arm64, "uv-aarch64-apple-darwin.tar.gz"),
    rule(Os::Mac, Arch::X64, "uv-x86_64-apple-darwin.tar.gz"),
];

const PNPM_ASSETS: &[AssetRule] = &[
    rule(Os::Windows, Arch::X64, "pnpm-win-x64.exe"),
    rule(Os::Windows, Arch::Arm64, "pnpm-win-arm64.exe"),
    rule(Os::Linux, Arch::X64, "pnpm-linux-x64"),
    rule(Os::Linux, Arch::Arm64, "pnpm-linux-arm64"),
    rule(Os::Mac, Arch::Arm64, "pnpm-macos-arm64"),
    rule(Os::Mac, Arch::X64, "pnpm-macos-x64"),
];

const BUN_ASSETS: &[AssetRule] = &[
    rule(Os::Windows, Arch::X64, "bun-windows-x64.zip"),
    rule(Os::Windows, Arch::Arm64, "bun-windows-x64.zip"),
    rule(Os::Linux, Arch::X64, "bun-linux-x64.zip"),
    rule(Os::Linux, Arch::Arm64, "bun-linux-aarch64.zip"),
    rule(Os::Mac, Arch::Arm64, "bun-darwin-aarch64.zip"),
    rule(Os::Mac, Arch::X64, "bun-darwin-x64.zip"),
];

const DENO_ASSETS: &[AssetRule] = &[
    rule(Os::Windows, Arch::X64, "deno-x86_64-pc-windows-msvc.zip"),
    rule(Os::Windows, Arch::Arm64, "deno-x86_64-pc-windows-msvc.zip"),
    rule(Os::Linux, Arch::X64, "deno-x86_64-unknown-linux-gnu.zip"),
    rule(Os::Linux, Arch::Arm64, "deno-aarch64-unknown-linux-gnu.zip"),
    rule(Os::Mac, Arch::Arm64, "deno-aarch64-apple-darwin.zip"),
    rule(Os::Mac, Arch::X64, "deno-x86_64-apple-darwin.zip"),
];

const STATIC_PHP_ASSETS: &[AssetRule] = &[
    rule(Os::Linux, Arch::X64, "php-*-cli-linux-x86_64.tar.gz"),
    rule(Os::Linux, Arch::Arm64, "php-*-cli-linux-aarch64.tar.gz"),
    rule(Os::Mac, Arch::Arm64, "php-*-cli-macos-aarch64.tar.gz"),
    rule(Os::Mac, Arch::X64, "php-*-cli-macos-x86_64.tar.gz"),
];

const RUBY_WIN_ASSETS: &[AssetRule] = &[
    rule(Os::Windows, Arch::X64, "rubyinstaller-devkit-*-x64.exe"),
    rule(Os::Windows, Arch::Arm64, "rubyinstaller-devkit-*-x64.exe"),
];

fn safe_dir_name(prefix: &str, version: &str) -> String {
    let cleaned = version.trim_start_matches('v').trim_start_matches("go");
    let safe: String =
        cleaned.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
    let safe = safe.trim_matches('-');
    if safe.is_empty() { prefix.to_owned() } else { format!("{prefix}-{safe}") }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

fn unpacked_single_root(pkg: &Path) -> PathBuf {
    let entries: Vec<PathBuf> = std::fs::read_dir(pkg)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    if entries.len() == 1 && entries[0].is_dir() {
        return entries[0].clone();
    }
    pkg.to_path_buf()
}

fn write_sdk_manifest(home: &Path, bin_dir: &Path, manifest: &SdkManifest) -> IdeResult<()> {
    let mut m = manifest.clone();
    m.bin_dir = bin_dir
        .strip_prefix(home)
        .map(|r| {
            let s = r.to_string_lossy().replace('\\', "/");
            if s.is_empty() { ".".to_owned() } else { s }
        })
        .unwrap_or_else(|_| ".".to_owned());
    std::fs::write(home.join(SDK_MANIFEST), serde_json::to_vec_pretty(&m).unwrap_or_default())?;
    Ok(())
}

fn register_and_log(path_dirs: &[PathBuf], env_vars: &[(String, String)], label: &str, emit: Emit) {
    let pairs: Vec<(&str, &str)> = env_vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    match ide_core::process::register_user_env(path_dirs, &pairs) {
        Ok(()) => {
            let mut names: Vec<&str> = vec!["PATH"];
            for (k, _) in &pairs {
                names.push(k);
            }
            emit(InstallEvent::Line {
                text: format!("Added {label} to environment variables ({}).", names.join(", ")),
            });
        }
        Err(err) => {
            emit(InstallEvent::Line {
                text: format!("Warning: could not update user environment variables: {err}"),
            });
        }
    }
}

fn proto(msg: impl Into<String>) -> IdeError {
    IdeError::new(ErrorKind::Protocol, msg)
}

#[derive(Deserialize)]
struct NodeRelease {
    version: String,
    lts: serde_json::Value,
}

pub fn pick_node_download(json: &str, lts_only: bool, platform: Platform) -> IdeResult<Download> {
    let releases: Vec<NodeRelease> =
        serde_json::from_str(json).map_err(|e| proto(format!("unexpected Node.js index: {e}")))?;
    let rel = releases
        .into_iter()
        .find(|r| !lts_only || matches!(&r.lts, serde_json::Value::String(s) if !s.is_empty()))
        .ok_or_else(|| proto("no matching Node.js release found"))?;
    let os = match platform.os {
        Os::Windows => "win",
        Os::Mac => "darwin",
        Os::Linux => "linux",
    };
    let arch = match platform.arch {
        Arch::X64 => "x64",
        Arch::Arm64 => "arm64",
    };
    let ext = if platform.os == Os::Windows { "zip" } else { "tar.gz" };
    let name = format!("node-{}-{os}-{arch}.{ext}", rel.version);
    let url = format!("https://nodejs.org/dist/{}/{name}", rel.version);
    let checksum_url = Some(format!("https://nodejs.org/dist/{}/SHASUMS256.txt", rel.version));
    Ok(Download {
        version: rel.version.trim_start_matches('v').to_owned(),
        name,
        url,
        size: None,
        sha256: None,
        checksum_url,
    })
}

#[derive(Deserialize)]
struct GoRelease {
    version: String,
    stable: bool,
    files: Vec<GoFile>,
}

#[derive(Deserialize)]
struct GoFile {
    filename: String,
    os: String,
    arch: String,
    sha256: String,
    size: Option<u64>,
    kind: String,
}

pub fn pick_go_download(json: &str, platform: Platform) -> IdeResult<Download> {
    let releases: Vec<GoRelease> =
        serde_json::from_str(json).map_err(|e| proto(format!("unexpected Go releases JSON: {e}")))?;
    let os = match platform.os {
        Os::Windows => "windows",
        Os::Mac => "darwin",
        Os::Linux => "linux",
    };
    let arch = match platform.arch {
        Arch::X64 => "amd64",
        Arch::Arm64 => "arm64",
    };
    for rel in releases.into_iter().filter(|r| r.stable) {
        if let Some(f) = rel.files.into_iter().find(|f| f.kind == "archive" && f.os == os && f.arch == arch) {
            return Ok(Download {
                version: rel.version.trim_start_matches("go").to_owned(),
                url: format!("https://dl.google.com/go/{}", f.filename),
                name: f.filename,
                size: f.size,
                sha256: (!f.sha256.is_empty()).then(|| f.sha256.to_ascii_lowercase()),
                checksum_url: None,
            });
        }
    }
    Err(IdeError::unavailable(format!("no Go archive found for {:?} {:?}", platform.os, platform.arch)))
}

pub fn pick_zig_download(json: &str, master: bool, platform: Platform) -> IdeResult<Download> {
    let root: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(json).map_err(|e| proto(format!("unexpected Zig download index: {e}")))?;
    let target = match (platform.os, platform.arch) {
        (Os::Windows, Arch::X64) => "x86_64-windows",
        (Os::Windows, Arch::Arm64) => "aarch64-windows",
        (Os::Linux, Arch::X64) => "x86_64-linux",
        (Os::Linux, Arch::Arm64) => "aarch64-linux",
        (Os::Mac, Arch::Arm64) => "aarch64-macos",
        (Os::Mac, Arch::X64) => "x86_64-macos",
    };
    let (ver_label, entry) = if master {
        let m = root.get("master").ok_or_else(|| proto("Zig index has no master entry"))?;
        let ver = m.get("version").and_then(|v| v.as_str()).unwrap_or("master").to_owned();
        (ver, m)
    } else {
        let mut versions: Vec<(&String, &serde_json::Value)> =
            root.iter().filter(|(k, v)| k.as_str() != "master" && v.get(target).is_some()).collect();
        versions.sort_by(|a, b| ide_core::tool_path::version_key(b.0).cmp(&ide_core::tool_path::version_key(a.0)));
        let (k, v) = versions
            .into_iter()
            .next()
            .ok_or_else(|| IdeError::unavailable(format!("no Zig build found for {target}")))?;
        (k.clone(), v)
    };
    let build = entry.get(target).ok_or_else(|| IdeError::unavailable(format!("no Zig build for {target}")))?;
    let url = build.get("tarball").and_then(|v| v.as_str()).ok_or_else(|| proto("missing Zig tarball URL"))?;
    let name = url.rsplit('/').next().unwrap_or("zig.zip").to_owned();
    let sha256 = build.get("shasum").and_then(|v| v.as_str()).map(str::to_ascii_lowercase).filter(|s| s.len() == 64);
    let size = build.get("size").and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok());
    Ok(Download { version: ver_label, name, url: url.to_owned(), size, sha256, checksum_url: None })
}

pub fn pick_php_windows_download(json: &str) -> IdeResult<Download> {
    let root: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(json).map_err(|e| proto(format!("unexpected PHP Windows releases JSON: {e}")))?;
    let mut branches: Vec<(&String, &serde_json::Value)> = root.iter().collect();
    branches.sort_by(|a, b| ide_core::tool_path::version_key(b.0).cmp(&ide_core::tool_path::version_key(a.0)));
    for (_, branch) in branches {
        let Some(obj) = branch.as_object() else { continue };
        let version = obj.get("version").and_then(|v| v.as_str()).unwrap_or("8").to_owned();
        // Prefer Non-Thread-Safe (`nts-*-x64`) for CLI / Composer, then Thread-Safe (`ts-*-x64`).
        let mut candidates: Vec<(&String, &serde_json::Value)> = obj
            .iter()
            .filter(|(k, v)| k.ends_with("-x64") && v.get("zip").and_then(|z| z.get("path")).is_some())
            .collect();
        candidates.sort_by_key(|(k, _)| if k.starts_with("nts-") { 0 } else { 1 });
        if let Some((_, entry)) = candidates.into_iter().next() {
            let zip = &entry["zip"];
            let path = zip["path"].as_str().ok_or_else(|| proto("missing PHP zip path"))?;
            let filename = path.rsplit('/').next().unwrap_or(path).to_owned();
            let sha256 = zip.get("sha256").and_then(|v| v.as_str()).map(str::to_ascii_lowercase).filter(|s| s.len() == 64);
            let url = format!("https://downloads.php.net/~windows/releases/{}", path.trim_start_matches('/'));
            return Ok(Download { version, name: filename, url, size: None, sha256, checksum_url: None });
        }
    }
    Err(IdeError::unavailable("no PHP Windows x64 archive found in releases.json"))
}

fn configure_windows_php_ini(php_dir: &Path) {
    let ini = php_dir.join("php.ini");
    if ini.is_file() {
        return;
    }
    let dev = php_dir.join("php.ini-development");
    let Ok(mut content) = std::fs::read_to_string(&dev) else {
        return;
    };
    content = content.replace(";extension_dir = \"ext\"", "extension_dir = \"ext\"");
    for ext in ["curl", "fileinfo", "mbstring", "openssl", "pdo_sqlite", "sqlite3", "zip"] {
        content = content.replace(&format!(";extension={ext}"), &format!("extension={ext}"));
    }
    let _ = std::fs::write(&ini, content);
}

#[derive(Deserialize)]
struct FlutterManifest {
    base_url: String,
    current_release: std::collections::HashMap<String, String>,
    releases: Vec<FlutterRelease>,
}

#[derive(Deserialize)]
struct FlutterRelease {
    hash: String,
    version: String,
    archive: String,
    #[serde(default)]
    dart_sdk_arch: Option<String>,
    #[serde(default)]
    sha256: Option<String>,
}

pub fn pick_flutter_download(json: &str, platform: Platform) -> IdeResult<Download> {
    let manifest: FlutterManifest =
        serde_json::from_str(json).map_err(|e| proto(format!("unexpected Flutter releases JSON: {e}")))?;
    let stable_hash = manifest.current_release.get("stable").cloned().unwrap_or_default();
    let want_arch = match platform.arch {
        Arch::Arm64 => "arm64",
        Arch::X64 => "x64",
    };
    let picked = manifest
        .releases
        .iter()
        .find(|r| r.hash == stable_hash && r.dart_sdk_arch.as_deref().is_none_or(|a| a == want_arch))
        .or_else(|| manifest.releases.iter().find(|r| r.hash == stable_hash))
        .ok_or_else(|| IdeError::unavailable("no stable Flutter release found for this platform"))?;
    let url = format!("{}/{}", manifest.base_url.trim_end_matches('/'), picked.archive.trim_start_matches('/'));
    let name = picked.archive.rsplit('/').next().unwrap_or("flutter.zip").to_owned();
    let sha256 = picked.sha256.as_deref().map(str::to_ascii_lowercase).filter(|s| s.len() == 64);
    Ok(Download { version: picked.version.clone(), name, url, size: None, sha256, checksum_url: None })
}

impl Tools {
    /// Downloads and configures a language SDK/runtime (`uv`, `node-lts`, `node-current`,
    /// `pnpm`, `bun`, `deno`, `rustup-stable`, `rustup-nightly`, `go`, `zig`, `zig-master`,
    /// `dotnet-lts`, `dotnet-sts`, `php`, `dart`, `flutter`, `maven`, `gradle`,
    /// `ruby-installer`, `ghcup`) and registers it in user environment variables (`PATH` +
    /// language-specific variables such as `GOROOT`, `DOTNET_ROOT`, `MAVEN_HOME`, `GRADLE_HOME`).
    pub fn download_toolchain(
        &self,
        provider_id: &str,
        location: Option<&Path>,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let provider = providers_for(self.platform, None)
            .into_iter()
            .find(|p| p.id == provider_id)
            .ok_or_else(|| {
                IdeError::invalid(format!(
                    "Toolchain provider `{provider_id}` is not available on {:?} {:?}",
                    self.platform.os, self.platform.arch
                ))
            })?;
        let _guard = InstallGuard::acquire(&format!("toolchain-{provider_id}"))?;
        let location = location.map(Path::to_path_buf).unwrap_or_else(|| self.root.join("sdks"));
        std::fs::create_dir_all(&location)?;

        match provider_id {
            "uv" => self.install_uv_and_python(&provider, &location, cancel, emit),
            "node-lts" => self.install_node(&provider, true, &location, cancel, emit),
            "node-current" => self.install_node(&provider, false, &location, cancel, emit),
            "pnpm" => self.install_single_github_tool(&provider, "pnpm/pnpm", PNPM_ASSETS, "pnpm", &location, cancel, emit),
            "bun" => self.install_single_github_tool(&provider, "oven-sh/bun", BUN_ASSETS, "bun", &location, cancel, emit),
            "deno" => self.install_single_github_tool(&provider, "denoland/deno", DENO_ASSETS, "deno", &location, cancel, emit),
            "rustup-stable" => self.install_rustup(&provider, "stable", cancel, emit),
            "rustup-nightly" => self.install_rustup(&provider, "nightly", cancel, emit),
            "go" => self.install_go_sdk(&provider, &location, cancel, emit),
            "zig" => self.install_zig_sdk(&provider, false, &location, cancel, emit),
            "zig-master" => self.install_zig_sdk(&provider, true, &location, cancel, emit),
            "dotnet-lts" => self.install_dotnet_sdk(&provider, "LTS", &location, cancel, emit),
            "dotnet-sts" => self.install_dotnet_sdk(&provider, "STS", &location, cancel, emit),
            "php" => self.install_php_and_composer(&provider, &location, cancel, emit),
            "dart" => self.install_dart_sdk(&provider, &location, cancel, emit),
            "flutter" => self.install_flutter_sdk(&provider, &location, cancel, emit),
            "maven" => self.install_maven_sdk(&provider, &location, cancel, emit),
            "gradle" => self.install_gradle_sdk(&provider, &location, cancel, emit),
            "ruby-installer" => self.install_ruby_windows(&provider, &location, cancel, emit),
            "ghcup" => self.install_ghcup(&provider, cancel, emit),
            _ => unreachable!("validated above"),
        }
    }

    fn unpack_archive_into(
        &self,
        http: &Http,
        dl: &Download,
        location: &Path,
        target: &Path,
        single_name: &str,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<()> {
        emit(InstallEvent::Line { text: format!("Downloading {} ({})", dl.name, dl.url) });
        let staging = location.join(format!(".staging-{}", target.file_name().unwrap_or_default().to_string_lossy()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        let res = (|| -> IdeResult<()> {
            let archive_path = fetch_verified(&self.downloads, http, dl, &staging, cancel, emit)?;
            emit(InstallEvent::Line { text: format!("Extracting {}…", dl.name) });
            let pkg = staging.join("pkg");
            archive::extract(&archive_path, archive::ArchiveKind::from_name(&dl.name), &pkg, single_name, cancel)?;
            if cancel.is_cancelled() {
                return Err(IdeError::cancelled());
            }
            if target.exists() {
                std::fs::remove_dir_all(target)?;
            }
            let src = unpacked_single_root(&pkg);
            std::fs::rename(&src, target)?;
            clear_quarantine(target);
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&staging);
        res
    }

    fn install_uv_and_python(
        &self,
        provider: &ToolchainProvider,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let http = Http::new(&self.system_path)?;
        emit(InstallEvent::Line { text: "Resolving the latest uv release…".into() });
        let dl = self.resolve_release(&http, ReleaseSource::GitHub { repo: "astral-sh/uv" }, UV_ASSETS, cancel)?;
        let target = location.join("uv");
        let bin_dir = target.join("bin");
        let uv_exe_name = self.platform.exe("uv");

        self.unpack_archive_into(&http, &dl, location, &target, &uv_exe_name, cancel, emit)?;
        std::fs::create_dir_all(&bin_dir)?;
        for name in ["uv", "uvx"] {
            let exe_name = self.platform.exe(name);
            if let Some(found) = archive::locate(&target, std::slice::from_ref(&exe_name))
                && found.parent() != Some(bin_dir.as_path())
            {
                let dest = bin_dir.join(&exe_name);
                let _ = std::fs::copy(&found, &dest);
                let _ = archive::make_executable(&dest);
            }
        }
        let uv_exe = bin_dir.join(&uv_exe_name);
        if !uv_exe.is_file() {
            return Err(IdeError::new(ErrorKind::Io, format!("`{uv_exe_name}` was not found after extracting {}", dl.name)));
        }

        let manifest = SdkManifest {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: provider.name.clone(),
            version: dl.version.clone(),
            bin_dir: "bin".into(),
            env_vars: Vec::new(),
        };
        write_sdk_manifest(&target, &bin_dir, &manifest)?;

        // Register uv on PATH right away, then install a managed Python via uv.
        register_and_log(std::slice::from_ref(&bin_dir), &[], "uv", emit);
        emit(InstallEvent::Line { text: "Installing managed Python via `uv python install`…".into() });
        let mut py_cmd = self.command(&uv_exe);
        py_cmd.args(["python", "install"]);
        if let Err(err) = run_streamed(py_cmd, cancel, emit) {
            if err.kind == ErrorKind::Cancelled {
                return Err(err);
            }
            emit(InstallEvent::Line {
                text: format!("Note: `uv python install` did not complete automatically ({}). uv will install Python on demand.", err.message),
            });
        }

        emit(InstallEvent::Line { text: format!("uv {} installed in {}", dl.version, target.display()) });
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: format!("uv {}", dl.version),
            version: dl.version,
            home: target.into(),
            bin_dir: bin_dir.into(),
            env_vars: Vec::new(),
        })
    }

    fn install_node(
        &self,
        provider: &ToolchainProvider,
        lts_only: bool,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let http = Http::new(&self.system_path)?;
        emit(InstallEvent::Line {
            text: format!("Resolving the latest {} release…", if lts_only { "Node.js LTS" } else { "Node.js" }),
        });
        let index = http.get_text("https://nodejs.org/dist/index.json", cancel)?;
        let dl = pick_node_download(&index, lts_only, self.platform)?;
        let target = location.join(safe_dir_name("node", &dl.version));
        let bin_dir = if self.platform.os == Os::Windows { target.clone() } else { target.join("bin") };
        let node_exe = bin_dir.join(self.platform.exe("node"));

        if node_exe.is_file() {
            emit(InstallEvent::Line { text: format!("Node.js {} is already installed in {}", dl.version, target.display()) });
        } else {
            self.unpack_archive_into(&http, &dl, location, &target, &self.platform.exe("node"), cancel, emit)?;
        }
        if !node_exe.is_file() {
            return Err(IdeError::new(ErrorKind::Io, format!("node executable not found in {}", target.display())));
        }
        let manifest = SdkManifest {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: provider.name.clone(),
            version: dl.version.clone(),
            bin_dir: String::new(),
            env_vars: Vec::new(),
        };
        write_sdk_manifest(&target, &bin_dir, &manifest)?;
        register_and_log(std::slice::from_ref(&bin_dir), &[], "Node.js", emit);
        emit(InstallEvent::Line { text: format!("Node.js {} installed in {}", dl.version, target.display()) });
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: format!("Node.js {}", dl.version),
            version: dl.version,
            home: target.into(),
            bin_dir: bin_dir.into(),
            env_vars: Vec::new(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn install_single_github_tool(
        &self,
        provider: &ToolchainProvider,
        repo: &'static str,
        assets: &[AssetRule],
        cmd_name: &str,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let http = Http::new(&self.system_path)?;
        emit(InstallEvent::Line { text: format!("Resolving the latest {} release…", provider.name) });
        let dl = self.resolve_release(&http, ReleaseSource::GitHub { repo }, assets, cancel)?;
        let target = location.join(cmd_name);
        let bin_dir = target.join("bin");
        let exe_name = self.platform.exe(cmd_name);

        self.unpack_archive_into(&http, &dl, location, &target, &exe_name, cancel, emit)?;
        std::fs::create_dir_all(&bin_dir)?;
        if let Some(found) = archive::locate(&target, std::slice::from_ref(&exe_name))
            && found.parent() != Some(bin_dir.as_path())
        {
            let dest = bin_dir.join(&exe_name);
            let _ = std::fs::copy(&found, &dest);
            let _ = archive::make_executable(&dest);
        }
        let final_exe = bin_dir.join(&exe_name);
        if !final_exe.is_file() {
            return Err(IdeError::new(ErrorKind::Io, format!("`{exe_name}` was not found in {}", target.display())));
        }
        let manifest = SdkManifest {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: provider.name.clone(),
            version: dl.version.clone(),
            bin_dir: "bin".into(),
            env_vars: Vec::new(),
        };
        write_sdk_manifest(&target, &bin_dir, &manifest)?;
        register_and_log(std::slice::from_ref(&bin_dir), &[], &provider.name, emit);
        emit(InstallEvent::Line { text: format!("{} {} installed in {}", provider.name, dl.version, target.display()) });
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: format!("{} {}", provider.name, dl.version),
            version: dl.version,
            home: target.into(),
            bin_dir: bin_dir.into(),
            env_vars: Vec::new(),
        })
    }

    fn install_rustup(
        &self,
        provider: &ToolchainProvider,
        channel: &str,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let http = Http::new(&self.system_path)?;
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|h| h.join(".cargo")))
            .ok_or_else(|| IdeError::new(ErrorKind::Io, "cannot determine user home directory for Cargo"))?;
        let cargo_bin = cargo_home.join("bin");
        let rustup_exe = cargo_bin.join(self.platform.exe("rustup"));

        if rustup_exe.is_file() {
            emit(InstallEvent::Line { text: format!("rustup is already installed; installing `{channel}` toolchain…") });
            let mut cmd = self.command(&rustup_exe);
            cmd.args(["toolchain", "install", channel, "--profile", "default"]);
            run_streamed(cmd, cancel, emit)?;
            let mut def = self.command(&rustup_exe);
            def.args(["default", channel]);
            let _ = run_streamed(def, cancel, emit);
        } else {
            let staging = self.root.join("sdks").join(".staging-rustup");
            let _ = std::fs::remove_dir_all(&staging);
            std::fs::create_dir_all(&staging)?;
            let res = (|| -> IdeResult<()> {
                if self.platform.os == Os::Windows {
                    let url = match self.platform.arch {
                        Arch::Arm64 => "https://win.rustup.rs/aarch64",
                        Arch::X64 => "https://win.rustup.rs/x86_64",
                    };
                    let installer = staging.join("rustup-init.exe");
                    emit(InstallEvent::Line { text: format!("Downloading rustup-init ({url})…") });
                    http.download(url, &installer, None, cancel, emit)?;
                    emit(InstallEvent::Line { text: format!("Installing Rust ({channel}) via rustup…") });
                    let mut cmd = self.command(&installer);
                    cmd.args(["-y", "--default-toolchain", channel, "--profile", "default", "--no-modify-path"]);
                    run_streamed(cmd, cancel, emit)?;
                } else {
                    let url = "https://sh.rustup.rs";
                    let script = staging.join("rustup-init.sh");
                    emit(InstallEvent::Line { text: format!("Downloading rustup installer ({url})…") });
                    http.download(url, &script, None, cancel, emit)?;
                    emit(InstallEvent::Line { text: format!("Installing Rust ({channel}) via rustup…") });
                    let mut cmd = self.command("sh");
                    cmd.arg(&script).args(["-y", "--default-toolchain", channel, "--profile", "default", "--no-modify-path"]);
                    run_streamed(cmd, cancel, emit)?;
                }
                Ok(())
            })();
            let _ = std::fs::remove_dir_all(&staging);
            res?;
        }

        register_and_log(std::slice::from_ref(&cargo_bin), &[], "Rust (Cargo)", emit);
        emit(InstallEvent::Line { text: format!("Rust ({channel}) is ready in {}", cargo_bin.display()) });
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: format!("Rust ({channel})"),
            version: channel.to_owned(),
            home: cargo_home.into(),
            bin_dir: cargo_bin.into(),
            env_vars: Vec::new(),
        })
    }

    fn install_go_sdk(
        &self,
        provider: &ToolchainProvider,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let http = Http::new(&self.system_path)?;
        emit(InstallEvent::Line { text: "Resolving the latest Go SDK release…".into() });
        let json = http.get_text("https://go.dev/dl/?mode=json", cancel)?;
        let dl = pick_go_download(&json, self.platform)?;
        let target = location.join(safe_dir_name("go", &dl.version));
        let bin_dir = target.join("bin");
        let go_exe = bin_dir.join(self.platform.exe("go"));

        if go_exe.is_file() {
            emit(InstallEvent::Line { text: format!("Go {} is already installed in {}", dl.version, target.display()) });
        } else {
            self.unpack_archive_into(&http, &dl, location, &target, &self.platform.exe("go"), cancel, emit)?;
        }
        if !go_exe.is_file() {
            return Err(IdeError::new(ErrorKind::Io, format!("go executable not found in {}", target.display())));
        }
        let env_vars = vec![("GOROOT".to_owned(), target.to_string_lossy().into_owned())];
        let manifest = SdkManifest {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: provider.name.clone(),
            version: dl.version.clone(),
            bin_dir: "bin".into(),
            env_vars: env_vars.clone(),
        };
        write_sdk_manifest(&target, &bin_dir, &manifest)?;
        let mut path_dirs = vec![bin_dir.clone()];
        if let Some(h) = home_dir() {
            path_dirs.push(h.join("go").join("bin"));
        }
        register_and_log(&path_dirs, &env_vars, "Go SDK", emit);
        emit(InstallEvent::Line { text: format!("Go {} installed in {}", dl.version, target.display()) });
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: format!("Go {}", dl.version),
            version: dl.version,
            home: target.into(),
            bin_dir: bin_dir.into(),
            env_vars,
        })
    }

    fn install_zig_sdk(
        &self,
        provider: &ToolchainProvider,
        master: bool,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let http = Http::new(&self.system_path)?;
        emit(InstallEvent::Line { text: "Resolving the latest Zig release…".into() });
        let json = http.get_text("https://ziglang.org/download/index.json", cancel)?;
        let dl = pick_zig_download(&json, master, self.platform)?;
        let target = location.join(safe_dir_name("zig", &dl.version));
        let zig_exe = target.join(self.platform.exe("zig"));

        if zig_exe.is_file() {
            emit(InstallEvent::Line { text: format!("Zig {} is already installed in {}", dl.version, target.display()) });
        } else {
            self.unpack_archive_into(&http, &dl, location, &target, &self.platform.exe("zig"), cancel, emit)?;
        }
        if !zig_exe.is_file() {
            return Err(IdeError::new(ErrorKind::Io, format!("zig executable not found in {}", target.display())));
        }
        let manifest = SdkManifest {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: provider.name.clone(),
            version: dl.version.clone(),
            bin_dir: ".".into(),
            env_vars: Vec::new(),
        };
        write_sdk_manifest(&target, &target, &manifest)?;
        register_and_log(std::slice::from_ref(&target), &[], "Zig", emit);
        emit(InstallEvent::Line { text: format!("Zig {} installed in {}", dl.version, target.display()) });
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: format!("Zig {}", dl.version),
            version: dl.version,
            home: target.clone().into(),
            bin_dir: target.into(),
            env_vars: Vec::new(),
        })
    }

    fn install_dotnet_sdk(
        &self,
        provider: &ToolchainProvider,
        channel: &str,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let http = Http::new(&self.system_path)?;
        let target = location.join("dotnet");
        std::fs::create_dir_all(&target)?;
        let staging = location.join(".staging-dotnet");
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;

        let res = (|| -> IdeResult<()> {
            if self.platform.os == Os::Windows {
                let script = staging.join("dotnet-install.ps1");
                emit(InstallEvent::Line { text: "Downloading Microsoft dotnet-install.ps1…".into() });
                http.download("https://dot.net/v1/dotnet-install.ps1", &script, None, cancel, emit)?;
                emit(InstallEvent::Line { text: format!("Installing .NET SDK ({channel}) into {}…", target.display()) });
                let mut cmd = Command::new("powershell");
                cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                    .arg(&script)
                    .args(["-Channel", channel, "-InstallDir"])
                    .arg(&target);
                run_streamed(cmd, cancel, emit)?;
            } else {
                let script = staging.join("dotnet-install.sh");
                emit(InstallEvent::Line { text: "Downloading Microsoft dotnet-install.sh…".into() });
                http.download("https://dot.net/v1/dotnet-install.sh", &script, None, cancel, emit)?;
                emit(InstallEvent::Line { text: format!("Installing .NET SDK ({channel}) into {}…", target.display()) });
                let mut cmd = self.command("sh");
                cmd.arg(&script).args(["--channel", channel, "--install-dir"]).arg(&target);
                run_streamed(cmd, cancel, emit)?;
            }
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&staging);
        res?;

        let dotnet_exe = target.join(self.platform.exe("dotnet"));
        if !dotnet_exe.is_file() {
            return Err(IdeError::new(ErrorKind::Io, format!("dotnet executable not found in {}", target.display())));
        }
        let env_vars = vec![("DOTNET_ROOT".to_owned(), target.to_string_lossy().into_owned())];
        let manifest = SdkManifest {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: provider.name.clone(),
            version: channel.to_owned(),
            bin_dir: ".".into(),
            env_vars: env_vars.clone(),
        };
        write_sdk_manifest(&target, &target, &manifest)?;
        let mut path_dirs = vec![target.clone()];
        if let Some(h) = home_dir() {
            path_dirs.push(h.join(".dotnet").join("tools"));
        }
        register_and_log(&path_dirs, &env_vars, ".NET SDK", emit);
        emit(InstallEvent::Line { text: format!(".NET SDK ({channel}) installed in {}", target.display()) });
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: format!(".NET SDK ({channel})"),
            version: channel.to_owned(),
            home: target.clone().into(),
            bin_dir: target.into(),
            env_vars,
        })
    }

    fn install_php_and_composer(
        &self,
        provider: &ToolchainProvider,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let http = Http::new(&self.system_path)?;
        let (target, bin_dir, version) = if self.platform.os == Os::Windows {
            emit(InstallEvent::Line { text: "Resolving the latest PHP Windows release…".into() });
            let json = http.get_text("https://downloads.php.net/~windows/releases/releases.json", cancel)?;
            let dl = pick_php_windows_download(&json)?;
            let target = location.join(safe_dir_name("php", &dl.version));
            if !target.join("php.exe").is_file() {
                self.unpack_archive_into(&http, &dl, location, &target, "php.exe", cancel, emit)?;
            }
            configure_windows_php_ini(&target);
            (target.clone(), target, dl.version)
        } else {
            emit(InstallEvent::Line { text: "Resolving the latest portable PHP release…".into() });
            let dl = self.resolve_release(
                &http,
                ReleaseSource::GitHub { repo: "crazywhalecc/static-php-cli" },
                STATIC_PHP_ASSETS,
                cancel,
            )?;
            let target = location.join(safe_dir_name("php", &dl.version));
            let bin_dir = target.join("bin");
            self.unpack_archive_into(&http, &dl, location, &target, "php", cancel, emit)?;
            std::fs::create_dir_all(&bin_dir)?;
            if let Some(found) = archive::locate(&target, &["php".into()])
                && found.parent() != Some(bin_dir.as_path())
            {
                let dest = bin_dir.join("php");
                let _ = std::fs::copy(&found, &dest);
                let _ = archive::make_executable(&dest);
            }
            (target, bin_dir, dl.version)
        };

        // Install Composer right next to `php` so Laravel / Symfony / PHP projects work immediately.
        let phar = bin_dir.join("composer.phar");
        emit(InstallEvent::Line { text: "Downloading Composer (composer.phar)…".into() });
        http.download("https://getcomposer.org/download/latest-stable/composer.phar", &phar, None, cancel, emit)?;
        if self.platform.os == Os::Windows {
            std::fs::write(bin_dir.join("composer.bat"), "@echo off\r\n\"%~dp0php.exe\" \"%~dp0composer.phar\" %*\r\n")?;
        } else {
            let script = bin_dir.join("composer");
            std::fs::write(&script, "#!/bin/sh\nDIR=\"$(cd \"$(dirname \"$0\")\" && pwd)\"\nexec \"$DIR/php\" \"$DIR/composer.phar\" \"$@\"\n")?;
            archive::make_executable(&script)?;
        }

        let manifest = SdkManifest {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: provider.name.clone(),
            version: version.clone(),
            bin_dir: String::new(),
            env_vars: Vec::new(),
        };
        write_sdk_manifest(&target, &bin_dir, &manifest)?;
        register_and_log(std::slice::from_ref(&bin_dir), &[], "PHP + Composer", emit);
        emit(InstallEvent::Line { text: format!("PHP {version} + Composer installed in {}", target.display()) });
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: format!("PHP {version} + Composer"),
            version,
            home: target.into(),
            bin_dir: bin_dir.into(),
            env_vars: Vec::new(),
        })
    }

    fn install_dart_sdk(
        &self,
        provider: &ToolchainProvider,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let http = Http::new(&self.system_path)?;
        let os = match self.platform.os {
            Os::Windows => "windows",
            Os::Mac => "macos",
            Os::Linux => "linux",
        };
        let arch = match self.platform.arch {
            Arch::X64 => "x64",
            Arch::Arm64 => "arm64",
        };
        let name = format!("dartsdk-{os}-{arch}-release.zip");
        let url = format!("https://storage.googleapis.com/dart-archive/channels/stable/release/latest/sdk/{name}");
        let dl = Download { version: "stable".into(), name, url, size: None, sha256: None, checksum_url: None };
        let target = location.join("dart-sdk");
        self.unpack_archive_into(&http, &dl, location, &target, &self.platform.exe("dart"), cancel, emit)?;
        let bin_dir = target.join("bin");
        let env_vars = vec![("DART_SDK".to_owned(), target.to_string_lossy().into_owned())];
        let manifest = SdkManifest {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: provider.name.clone(),
            version: "stable".into(),
            bin_dir: "bin".into(),
            env_vars: env_vars.clone(),
        };
        write_sdk_manifest(&target, &bin_dir, &manifest)?;
        register_and_log(std::slice::from_ref(&bin_dir), &env_vars, "Dart SDK", emit);
        emit(InstallEvent::Line { text: format!("Dart SDK installed in {}", target.display()) });
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: "Dart SDK (stable)".into(),
            version: "stable".into(),
            home: target.into(),
            bin_dir: bin_dir.into(),
            env_vars,
        })
    }

    fn install_flutter_sdk(
        &self,
        provider: &ToolchainProvider,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let http = Http::new(&self.system_path)?;
        let os = match self.platform.os {
            Os::Windows => "windows",
            Os::Mac => "macos",
            Os::Linux => "linux",
        };
        emit(InstallEvent::Line { text: "Resolving the latest stable Flutter SDK release…".into() });
        let json = http.get_text(
            &format!("https://storage.googleapis.com/flutter_infra_release/releases/releases_{os}.json"),
            cancel,
        )?;
        let dl = pick_flutter_download(&json, self.platform)?;
        let target = location.join("flutter");
        self.unpack_archive_into(&http, &dl, location, &target, "flutter", cancel, emit)?;
        let bin_dir = target.join("bin");
        let manifest = SdkManifest {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: provider.name.clone(),
            version: dl.version.clone(),
            bin_dir: "bin".into(),
            env_vars: Vec::new(),
        };
        write_sdk_manifest(&target, &bin_dir, &manifest)?;
        register_and_log(std::slice::from_ref(&bin_dir), &[], "Flutter SDK", emit);
        emit(InstallEvent::Line { text: format!("Flutter {} installed in {}", dl.version, target.display()) });
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: format!("Flutter {}", dl.version),
            version: dl.version,
            home: target.into(),
            bin_dir: bin_dir.into(),
            env_vars: Vec::new(),
        })
    }

    fn install_maven_sdk(
        &self,
        provider: &ToolchainProvider,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let http = Http::new(&self.system_path)?;
        emit(InstallEvent::Line { text: "Resolving the latest Apache Maven release…".into() });
        let meta = http.get_text("https://repo1.maven.org/maven2/org/apache/maven/apache-maven/maven-metadata.xml", cancel)?;
        let mut versions: Vec<String> = Vec::new();
        let mut rest = meta.as_str();
        while let Some(i) = rest.find("<version>") {
            let tail = &rest[i + 9..];
            let Some(end) = tail.find("</version>") else { break };
            let v = tail[..end].trim();
            if v.starts_with("3.") && !v.contains('-') {
                versions.push(v.to_owned());
            }
            rest = &tail[end + 10..];
        }
        versions.sort_by(|a, b| ide_core::tool_path::version_key(b).cmp(&ide_core::tool_path::version_key(a)));
        let version = versions.into_iter().next().unwrap_or_else(|| "3.9.9".to_owned());
        let name = format!("apache-maven-{version}-bin.zip");
        let url = format!("https://repo1.maven.org/maven2/org/apache/maven/apache-maven/{version}/{name}");
        let dl = Download { version: version.clone(), name, url, size: None, sha256: None, checksum_url: None };
        let target = location.join(safe_dir_name("maven", &version));
        let bin_dir = target.join("bin");
        if !bin_dir.is_dir() {
            self.unpack_archive_into(&http, &dl, location, &target, "mvn", cancel, emit)?;
        }
        let env_vars = vec![("MAVEN_HOME".to_owned(), target.to_string_lossy().into_owned())];
        let manifest = SdkManifest {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: provider.name.clone(),
            version: version.clone(),
            bin_dir: "bin".into(),
            env_vars: env_vars.clone(),
        };
        write_sdk_manifest(&target, &bin_dir, &manifest)?;
        register_and_log(std::slice::from_ref(&bin_dir), &env_vars, "Apache Maven", emit);
        emit(InstallEvent::Line { text: format!("Apache Maven {version} installed in {}", target.display()) });
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: format!("Apache Maven {version}"),
            version,
            home: target.into(),
            bin_dir: bin_dir.into(),
            env_vars,
        })
    }

    fn install_gradle_sdk(
        &self,
        provider: &ToolchainProvider,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let http = Http::new(&self.system_path)?;
        emit(InstallEvent::Line { text: "Resolving the latest Gradle release…".into() });
        let json = http.get_text("https://services.gradle.org/versions/current", cancel)?;
        let v: serde_json::Value =
            serde_json::from_str(&json).map_err(|e| proto(format!("unexpected Gradle metadata: {e}")))?;
        let version = v["version"].as_str().ok_or_else(|| proto("missing Gradle version"))?.to_owned();
        let url = v["downloadUrl"].as_str().ok_or_else(|| proto("missing Gradle downloadUrl"))?.to_owned();
        let name = url.rsplit('/').next().unwrap_or("gradle-bin.zip").to_owned();
        let dl = Download { version: version.clone(), name, url, size: None, sha256: None, checksum_url: None };
        let target = location.join(safe_dir_name("gradle", &version));
        let bin_dir = target.join("bin");
        if !bin_dir.is_dir() {
            self.unpack_archive_into(&http, &dl, location, &target, "gradle", cancel, emit)?;
        }
        let env_vars = vec![("GRADLE_HOME".to_owned(), target.to_string_lossy().into_owned())];
        let manifest = SdkManifest {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: provider.name.clone(),
            version: version.clone(),
            bin_dir: "bin".into(),
            env_vars: env_vars.clone(),
        };
        write_sdk_manifest(&target, &bin_dir, &manifest)?;
        register_and_log(std::slice::from_ref(&bin_dir), &env_vars, "Gradle", emit);
        emit(InstallEvent::Line { text: format!("Gradle {version} installed in {}", target.display()) });
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: format!("Gradle {version}"),
            version,
            home: target.into(),
            bin_dir: bin_dir.into(),
            env_vars,
        })
    }

    fn install_ruby_windows(
        &self,
        provider: &ToolchainProvider,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let http = Http::new(&self.system_path)?;
        emit(InstallEvent::Line { text: "Resolving the latest RubyInstaller2 release…".into() });
        let dl = self.resolve_release(
            &http,
            ReleaseSource::GitHub { repo: "oneclick/rubyinstaller2" },
            RUBY_WIN_ASSETS,
            cancel,
        )?;
        let target = location.join(safe_dir_name("ruby", &dl.version));
        let staging = location.join(".staging-ruby");
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        let installer = staging.join(&dl.name);
        let res = (|| -> IdeResult<()> {
            emit(InstallEvent::Line { text: format!("Downloading {}…", dl.name) });
            http.download(&dl.url, &installer, dl.size, cancel, emit)?;
            emit(InstallEvent::Line { text: format!("Installing Ruby {} into {}…", dl.version, target.display()) });
            let mut cmd = self.command(&installer);
            cmd.args([
                "/SILENT",
                "/CURRENTUSER",
                "/NORESTART",
                &format!("/DIR={}", target.display()),
                "/TASKS=modpath",
            ]);
            run_streamed(cmd, cancel, emit)?;
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&staging);
        res?;

        let bin_dir = target.join("bin");
        register_and_log(std::slice::from_ref(&bin_dir), &[], "Ruby", emit);
        emit(InstallEvent::Line { text: format!("Ruby {} installed in {}", dl.version, target.display()) });
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: format!("Ruby {}", dl.version),
            version: dl.version,
            home: target.into(),
            bin_dir: bin_dir.into(),
            env_vars: Vec::new(),
        })
    }

    fn install_ghcup(
        &self,
        provider: &ToolchainProvider,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedToolchain> {
        let home = home_dir().ok_or_else(|| IdeError::new(ErrorKind::Io, "cannot determine user home directory"))?;
        emit(InstallEvent::Line { text: "Installing Haskell (GHC + Cabal) via GHCup…".into() });
        if self.platform.os == Os::Windows {
            let mut cmd = Command::new("powershell");
            cmd.args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-Command",
                "$env:BOOTSTRAP_HASKELL_NONINTERACTIVE='1'; Invoke-Expression (Invoke-RestMethod https://www.haskell.org/ghcup/sh/bootstrap-haskell.ps1)",
            ]);
            run_streamed(cmd, cancel, emit)?;
        } else {
            let mut cmd = self.command("sh");
            cmd.args(["-c", "BOOTSTRAP_HASKELL_NONINTERACTIVE=1 curl --proto '=https' --tlsv1.2 -sSf https://get-ghcup.haskell.org | sh"]);
            run_streamed(cmd, cancel, emit)?;
        }
        let ghcup_bin = home.join(".ghcup").join("bin");
        let cabal_bin = home.join(".cabal").join("bin");
        register_and_log(&[ghcup_bin.clone(), cabal_bin], &[], "Haskell (GHCup)", emit);
        Ok(DownloadedToolchain {
            id: provider.id.clone(),
            runtime: provider.runtime.clone(),
            name: "Haskell (GHCup)".into(),
            version: "latest".into(),
            home: home.join(".ghcup").into(),
            bin_dir: ghcup_bin.into(),
            env_vars: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIN_X64: Platform = Platform { os: Os::Windows, arch: Arch::X64 };
    const LINUX_X64: Platform = Platform { os: Os::Linux, arch: Arch::X64 };
    const MAC_ARM: Platform = Platform { os: Os::Mac, arch: Arch::Arm64 };

    #[test]
    fn picks_node_lts_and_current() {
        let json = r#"[
            {"version":"v25.1.0","lts":false},
            {"version":"v24.11.0","lts":"Krypton"},
            {"version":"v22.20.0","lts":"Jod"}
        ]"#;
        let lts = pick_node_download(json, true, WIN_X64).unwrap();
        assert_eq!(lts.version, "24.11.0");
        assert_eq!(lts.name, "node-v24.11.0-win-x64.zip");
        let cur = pick_node_download(json, false, MAC_ARM).unwrap();
        assert_eq!(cur.version, "25.1.0");
        assert_eq!(cur.name, "node-v25.1.0-darwin-arm64.tar.gz");
    }

    #[test]
    fn picks_go_archive() {
        let json = r#"[{"version":"go1.25.3","stable":true,"files":[
            {"filename":"go1.25.3.windows-amd64.msi","os":"windows","arch":"amd64","sha256":"aa","size":10,"kind":"installer"},
            {"filename":"go1.25.3.windows-amd64.zip","os":"windows","arch":"amd64","sha256":"bb","size":20,"kind":"archive"}
        ]}]"#;
        let dl = pick_go_download(json, WIN_X64).unwrap();
        assert_eq!(dl.version, "1.25.3");
        assert_eq!(dl.name, "go1.25.3.windows-amd64.zip");
        assert_eq!(dl.url, "https://dl.google.com/go/go1.25.3.windows-amd64.zip");
    }

    #[test]
    fn picks_zig_stable_and_master() {
        let json = r#"{
            "master":{"version":"0.16.0-dev.1","x86_64-windows":{"tarball":"https://ziglang.org/builds/zig-master.zip","shasum":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","size":"100"}},
            "0.14.1":{"x86_64-windows":{"tarball":"https://ziglang.org/download/0.14.1/zig-x86_64-windows-0.14.1.zip","shasum":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","size":"90"}},
            "0.15.0":{"x86_64-windows":{"tarball":"https://ziglang.org/download/0.15.0/zig-x86_64-windows-0.15.0.zip","shasum":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","size":"95"}}
        }"#;
        let stable = pick_zig_download(json, false, WIN_X64).unwrap();
        assert_eq!(stable.version, "0.15.0");
        assert!(stable.url.ends_with("0.15.0.zip"));
        let master = pick_zig_download(json, true, WIN_X64).unwrap();
        assert_eq!(master.version, "0.16.0-dev.1");
    }

    #[test]
    fn picks_php_windows_nts_x64() {
        let json = r#"{
            "8.3":{"version":"8.3.20","nts-vs16-x64":{"zip":{"path":"php-8.3.20-nts-Win32-vs16-x64.zip"}}},
            "8.4":{"version":"8.4.5","ts-vs17-x64":{"zip":{"path":"php-8.4.5-Win32-vs17-x64.zip"}},"nts-vs17-x64":{"zip":{"path":"php-8.4.5-nts-Win32-vs17-x64.zip"}}}
        }"#;
        let dl = pick_php_windows_download(json).unwrap();
        assert_eq!(dl.version, "8.4.5");
        assert_eq!(dl.name, "php-8.4.5-nts-Win32-vs17-x64.zip");
    }

    #[test]
    fn filters_providers_by_runtime_or_program() {
        assert!(!providers_for(WIN_X64, Some("python")).is_empty());
        assert!(providers_for(WIN_X64, Some("uv")).iter().any(|p| p.id == "uv"));
        assert!(providers_for(WIN_X64, Some("node")).iter().any(|p| p.id == "node-lts"));
        assert!(providers_for(WIN_X64, Some("cargo")).iter().any(|p| p.id == "rustup-stable"));
        assert!(providers_for(WIN_X64, Some("dotnet")).iter().any(|p| p.id == "dotnet-lts"));
        assert!(providers_for(WIN_X64, Some("composer")).iter().any(|p| p.id == "php"));
        assert!(providers_for(LINUX_X64, Some("ruby")).is_empty());
    }
}
