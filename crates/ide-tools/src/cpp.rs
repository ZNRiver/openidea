//! C/C++ compiler toolchain downloads (MinGW-w64, LLVM/Clang + standalone Windows SDK
//! without Visual Studio via `xwin`, LLVM-MinGW, Visual Studio Build Tools).
//!
//! Downloaded toolchains are unpacked into `<tools>/cpp/<provider>-<version>` (or a
//! chosen location) with an `openidea-toolchain.json` manifest so SDK discovery and
//! CMake / clangd integration pick them up automatically.

use crate::platform::{Arch, AssetRule, Os, Platform, rule};
use crate::process::{Emit, Http, run_quiet, run_streamed};
use crate::registry::ReleaseSource;
use crate::{InstallEvent, InstallGuard, Tools, archive, clear_quarantine, fetch_verified};
use ide_core::error::ErrorKind;
use ide_core::{CancellationToken, IdeError, IdeResult};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub const MINGW_WINLIBS: &str = "mingw-winlibs";
pub const LLVM_WINSDK: &str = "llvm-winsdk";
pub const LLVM_MINGW: &str = "llvm-mingw";
pub const MSVC_BUILDTOOLS: &str = "msvc-buildtools";
pub const LLVM_UNIX: &str = "llvm";

pub const TOOLCHAIN_MANIFEST: &str = "openidea-toolchain.json";

const VS_BUILDTOOLS_URL: &str = "https://aka.ms/vs/17/release/vs_buildtools.exe";

/// A C/C++ compiler distribution the IDE can download.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct CppProvider {
    pub id: String,
    pub name: String,
    pub description: String,
    /// Downloads/bundles the Windows SDK and CRT (without needing Visual Studio IDE for LLVM).
    pub includes_windows_sdk: bool,
    /// Runs an official system installer that prompts for administrator elevation.
    pub requires_admin: bool,
    pub recommended: bool,
}

/// A C/C++ toolchain the IDE downloaded or installed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DownloadedCppToolchain {
    pub home: ide_core::IpcPath,
    pub bin_dir: ide_core::IpcPath,
    pub provider: String,
    pub version: String,
    /// Standalone Windows SDK + CRT sysroot directory (for `llvm-winsdk`).
    pub winsysroot: Option<ide_core::IpcPath>,
}

/// Metadata written to `<toolchain>/openidea-toolchain.json` so `ide-sdk` knows how to
/// configure `PATH`, `CC`, `CXX`, `INCLUDE`, `LIB`, CMake and `clangd`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CppToolchainManifest {
    pub provider: String,
    pub name: String,
    pub version: String,
    /// Relative to the toolchain root (e.g. `bin` or `mingw64/bin`).
    pub bin_dir: String,
    pub c_compiler: String,
    pub cxx_compiler: String,
    /// Relative path to the standalone Windows SDK + CRT sysroot (e.g. `winsysroot`).
    #[serde(default)]
    pub winsysroot: Option<String>,
    /// Target architecture (`x64` or `arm64`) used inside the sysroot.
    #[serde(default)]
    pub arch: Option<String>,
}

/// Where downloaded C/C++ toolchains go by default.
pub fn cpp_dir(app_data: &Path) -> PathBuf {
    crate::tools_dir(app_data).join("cpp")
}

/// Providers offered on the current machine.
pub fn providers() -> Vec<CppProvider> {
    providers_for(Platform::current())
}

/// Providers offered on `platform` (pure for testing).
pub fn providers_for(platform: Platform) -> Vec<CppProvider> {
    match platform.os {
        Os::Windows => vec![
            CppProvider {
                id: LLVM_WINSDK.into(),
                name: "LLVM / Clang + Windows SDK (Standalone)".into(),
                description: "Official LLVM/Clang and LLD paired with the Microsoft Windows SDK & CRT downloaded directly via xwin — no Visual Studio installation required.".into(),
                includes_windows_sdk: true,
                requires_admin: false,
                recommended: true,
            },
            CppProvider {
                id: MINGW_WINLIBS.into(),
                name: "MinGW-w64 (GCC / WinLibs)".into(),
                description: "Portable GCC, G++, MinGW-w64 UCRT headers & libraries, GDB and Make. Self-contained, no Visual Studio required.".into(),
                includes_windows_sdk: false,
                requires_admin: false,
                recommended: false,
            },
            CppProvider {
                id: LLVM_MINGW.into(),
                name: "LLVM-MinGW (Clang + UCRT)".into(),
                description: "Portable LLVM/Clang, LLD and LLDB with MinGW-w64 UCRT headers & libraries (native x64 and ARM64).".into(),
                includes_windows_sdk: false,
                requires_admin: false,
                recommended: false,
            },
            CppProvider {
                id: MSVC_BUILDTOOLS.into(),
                name: "Visual Studio Build Tools (MSVC + Windows SDK)".into(),
                description: "Downloads and runs Microsoft's official Visual Studio Build Tools installer with the C++ workload (cl.exe + Windows SDK).".into(),
                includes_windows_sdk: true,
                requires_admin: true,
                recommended: false,
            },
        ],
        Os::Linux | Os::Mac => vec![CppProvider {
            id: LLVM_UNIX.into(),
            name: "LLVM / Clang".into(),
            description: "Official prebuilt LLVM/Clang and LLD compiler toolchain archive.".into(),
            includes_windows_sdk: false,
            requires_admin: false,
            recommended: true,
        }],
    }
}

const WINLIBS_ASSETS: &[AssetRule] = &[
    rule(Os::Windows, Arch::X64, "winlibs-x86_64-posix-seh-gcc-*-mingw-w64ucrt-*.zip"),
    rule(Os::Windows, Arch::X64, "winlibs-x86_64-posix-seh-gcc-*-ucrt-*.zip"),
    rule(Os::Windows, Arch::Arm64, "winlibs-x86_64-posix-seh-gcc-*-mingw-w64ucrt-*.zip"),
    rule(Os::Windows, Arch::Arm64, "winlibs-x86_64-posix-seh-gcc-*-ucrt-*.zip"),
];

const LLVM_MINGW_ASSETS: &[AssetRule] = &[
    rule(Os::Windows, Arch::Arm64, "llvm-mingw-*-ucrt-aarch64.zip"),
    rule(Os::Windows, Arch::X64, "llvm-mingw-*-ucrt-x86_64.zip"),
];

const LLVM_ASSETS: &[AssetRule] = &[
    rule(Os::Windows, Arch::Arm64, "clang+llvm-*-aarch64-pc-windows-msvc.tar.xz"),
    rule(Os::Windows, Arch::Arm64, "LLVM-*-woa64.tar.xz"),
    rule(Os::Windows, Arch::X64, "clang+llvm-*-x86_64-pc-windows-msvc.tar.xz"),
    rule(Os::Windows, Arch::X64, "LLVM-*-win64.tar.xz"),
    rule(Os::Linux, Arch::Arm64, "LLVM-*-Linux-ARM64.tar.xz"),
    rule(Os::Linux, Arch::Arm64, "clang+llvm-*-aarch64-linux-gnu*.tar.xz"),
    rule(Os::Linux, Arch::X64, "LLVM-*-Linux-X64.tar.xz"),
    rule(Os::Linux, Arch::X64, "clang+llvm-*-x86_64-linux-gnu*.tar.xz"),
    rule(Os::Mac, Arch::Arm64, "LLVM-*-macOS-ARM64.tar.xz"),
    rule(Os::Mac, Arch::Arm64, "clang+llvm-*-arm64-apple-darwin*.tar.xz"),
    rule(Os::Mac, Arch::X64, "LLVM-*-macOS-X64.tar.xz"),
    rule(Os::Mac, Arch::X64, "clang+llvm-*-x86_64-apple-darwin*.tar.xz"),
];

const XWIN_ASSETS: &[AssetRule] = &[
    rule(Os::Windows, Arch::Arm64, "xwin-*-aarch64-pc-windows-msvc.tar.gz"),
    rule(Os::Windows, Arch::Arm64, "xwin-*-x86_64-pc-windows-msvc.tar.gz"),
    rule(Os::Windows, Arch::X64, "xwin-*-x86_64-pc-windows-msvc.tar.gz"),
    rule(Os::Linux, Arch::X64, "xwin-*-x86_64-unknown-linux-musl.tar.gz"),
    rule(Os::Mac, Arch::Arm64, "xwin-*-aarch64-apple-darwin.tar.gz"),
    rule(Os::Mac, Arch::X64, "xwin-*-x86_64-apple-darwin.tar.gz"),
];

/// Safe folder name for a downloaded C++ toolchain, e.g. `llvm-winsdk-20.1.0`.
pub fn toolchain_dir_name(provider: &str, version: &str) -> String {
    let cleaned = version.trim_start_matches("llvmorg-").trim_start_matches('v');
    let safe: String =
        cleaned.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
    let safe = safe.trim_matches('-');
    if safe.is_empty() { provider.to_owned() } else { format!("{provider}-{safe}") }
}

/// Include and library directories discovered inside a `xwin --use-winsysroot-style` directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WinSysrootLayout {
    pub msvc_version: String,
    pub sdk_version: String,
    pub arch: String,
    pub include_dirs: Vec<PathBuf>,
    pub lib_dirs: Vec<PathBuf>,
}

/// Inspects a `xwin --use-winsysroot-style` directory and resolves the MSVC CRT and Windows SDK
/// include & library paths for `arch` (`"x64"` or `"arm64"`).
pub fn inspect_winsysroot(winsysroot: &Path, arch: &str) -> Option<WinSysrootLayout> {
    let newest_dir = |parent: &Path| -> Option<(String, PathBuf)> {
        let mut dirs: Vec<(String, PathBuf)> = std::fs::read_dir(parent)
            .ok()?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .collect();
        dirs.sort_by(|a, b| ide_core::tool_path::version_key(&b.0).cmp(&ide_core::tool_path::version_key(&a.0)));
        dirs.into_iter().next()
    };

    let (msvc_version, msvc_root) = newest_dir(&winsysroot.join("VC").join("Tools").join("MSVC"))?;
    let sdk_base = winsysroot.join("Windows Kits").join("10");
    let (sdk_version, sdk_include_root) = newest_dir(&sdk_base.join("Include"))?;
    let sdk_lib_root = sdk_base.join("Lib").join(&sdk_version);

    let mut include_dirs = Vec::new();
    let msvc_inc = msvc_root.join("include");
    if msvc_inc.is_dir() {
        include_dirs.push(msvc_inc);
    }
    for sub in ["ucrt", "um", "shared", "winrt", "cppwinrt"] {
        let dir = sdk_include_root.join(sub);
        if dir.is_dir() {
            include_dirs.push(dir);
        }
    }

    let mut lib_dirs = Vec::new();
    let msvc_lib = msvc_root.join("lib").join(arch);
    if msvc_lib.is_dir() {
        lib_dirs.push(msvc_lib);
    }
    for sub in ["ucrt", "um"] {
        let dir = sdk_lib_root.join(sub).join(arch);
        if dir.is_dir() {
            lib_dirs.push(dir);
        }
    }

    if include_dirs.is_empty() || lib_dirs.is_empty() {
        return None;
    }

    Some(WinSysrootLayout { msvc_version, sdk_version, arch: arch.to_owned(), include_dirs, lib_dirs })
}

/// Writes portable Clang `.cfg` files inside `bin_dir` and a `toolchain.cmake` file inside
/// `toolchain_root` pointing at `<toolchain_root>/winsysroot` so `clang`, `clang++`,
/// `clang-cl` and CMake use the standalone Windows SDK without Visual Studio.
pub fn write_llvm_winsdk_configs(
    toolchain_root: &Path,
    bin_dir: &Path,
    winsysroot: &Path,
    arch: &str,
) -> IdeResult<()> {
    let layout = inspect_winsysroot(winsysroot, arch).ok_or_else(|| {
        IdeError::new(
            ErrorKind::Io,
            format!("standalone Windows SDK at {} is missing MSVC or Windows Kits headers/libraries", winsysroot.display()),
        )
    })?;

    // Determine relative path from `bin_dir` to `winsysroot` so `<CFGDIR>` works even when portable.
    let depth = bin_dir.strip_prefix(toolchain_root).map(|r| r.components().count()).unwrap_or(1);
    let mut prefix = String::from("<CFGDIR>");
    for _ in 0..depth {
        prefix.push_str("/..");
    }
    let rel_sysroot = format!("{prefix}/winsysroot");

    let clang_cl_cfg = format!("-fuse-ld=lld\n/winsysroot\n{rel_sysroot}\n");
    std::fs::write(bin_dir.join("clang-cl.cfg"), clang_cl_cfg)?;

    // GNU-style `clang` and `clang++` read `clang.cfg` / `clang++.cfg` from their own directory.
    let mut gnu_cfg = String::from("-fuse-ld=lld\n");
    for inc in &layout.include_dirs {
        if let Ok(rel) = inc.strip_prefix(winsysroot) {
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            gnu_cfg.push_str(&format!("\"-isystem{rel_sysroot}/{rel_s}\"\n"));
        }
    }
    for lib in &layout.lib_dirs {
        if let Ok(rel) = lib.strip_prefix(winsysroot) {
            let rel_s = rel.to_string_lossy().replace('\\', "/");
            gnu_cfg.push_str(&format!("\"-L{rel_sysroot}/{rel_s}\"\n"));
        }
    }
    std::fs::write(bin_dir.join("clang.cfg"), &gnu_cfg)?;
    std::fs::write(bin_dir.join("clang++.cfg"), &gnu_cfg)?;

    let target_triple = if arch == "arm64" { "aarch64-pc-windows-msvc" } else { "x86_64-pc-windows-msvc" };
    let bin_rel = bin_dir
        .strip_prefix(toolchain_root)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| "bin".to_owned());
    let cmake_toolchain = format!(
        "# Generated by OpenIDEA for standalone LLVM + Windows SDK (no Visual Studio required)\n\
         set(CMAKE_SYSTEM_NAME Windows)\n\
         set(CMAKE_C_COMPILER \"${{CMAKE_CURRENT_LIST_DIR}}/{bin_rel}/clang.exe\")\n\
         set(CMAKE_CXX_COMPILER \"${{CMAKE_CURRENT_LIST_DIR}}/{bin_rel}/clang++.exe\")\n\
         set(CMAKE_C_COMPILER_TARGET \"{target_triple}\")\n\
         set(CMAKE_CXX_COMPILER_TARGET \"{target_triple}\")\n\
         set(CMAKE_LINKER \"${{CMAKE_CURRENT_LIST_DIR}}/{bin_rel}/lld-link.exe\")\n"
    );
    std::fs::write(toolchain_root.join("toolchain.cmake"), cmake_toolchain)?;
    Ok(())
}

fn find_compiler_bin(root: &Path, compiler_exe: &str) -> Option<PathBuf> {
    let found = archive::locate(root, &[compiler_exe.to_owned()])?;
    found.parent().map(Path::to_path_buf)
}

/// Moves the single top-level extracted directory when the compiler lives inside it,
/// otherwise the whole `pkg` folder.
fn unpacked_root(pkg: &Path, bin_dir: &Path) -> PathBuf {
    let entries: Vec<PathBuf> = std::fs::read_dir(pkg)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    if entries.len() == 1 && entries[0].is_dir() && bin_dir.starts_with(&entries[0]) {
        return entries[0].clone();
    }
    pkg.to_path_buf()
}

fn winsysroot_arch(arch: Arch) -> &'static str {
    match arch {
        Arch::Arm64 => "arm64",
        Arch::X64 => "x64",
    }
}

fn xwin_arch_arg(arch: Arch) -> &'static str {
    match arch {
        Arch::Arm64 => "aarch64",
        Arch::X64 => "x86_64",
    }
}

/// Locates an installed Visual Studio Build Tools / Visual Studio instance with C++ tools via `vswhere.exe`.
fn locate_vs_installation() -> Option<PathBuf> {
    let pf86 = std::env::var_os("ProgramFiles(x86)").map(PathBuf::from)?;
    let vswhere = pf86.join("Microsoft Visual Studio").join("Installer").join("vswhere.exe");
    if !vswhere.is_file() {
        return None;
    }
    let mut cmd = Command::new(vswhere);
    cmd.args([
        "-latest",
        "-products",
        "*",
        "-requires",
        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
        "-property",
        "installationPath",
    ]);
    let (ok, out) = run_quiet(cmd, Duration::from_secs(15))?;
    if !ok {
        return None;
    }
    let path = out.lines().map(str::trim).find(|l| !l.is_empty())?;
    let p = PathBuf::from(path);
    p.is_dir().then_some(p)
}

fn find_msvc_bin(vs_root: &Path, arch: Arch) -> Option<(PathBuf, String)> {
    let msvc_base = vs_root.join("VC").join("Tools").join("MSVC");
    let mut versions: Vec<(String, PathBuf)> = std::fs::read_dir(&msvc_base)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    versions.sort_by(|a, b| ide_core::tool_path::version_key(&b.0).cmp(&ide_core::tool_path::version_key(&a.0)));
    let subdirs = match arch {
        Arch::Arm64 => &["Hostarm64/arm64", "Hostx64/arm64", "Hostx64/x64"][..],
        Arch::X64 => &["Hostx64/x64", "Hostx86/x64", "Hostx86/x86"][..],
    };
    for (version, dir) in versions {
        for sub in subdirs {
            let bin = dir.join("bin").join(sub);
            if bin.join("cl.exe").is_file() {
                return Some((bin, version));
            }
        }
    }
    None
}

impl Tools {
    /// Downloads and configures a C/C++ compiler toolchain (`mingw-winlibs`, `llvm-winsdk`,
    /// `llvm-mingw`, `msvc-buildtools`, or `llvm`) into `location` (default: `<tools>/cpp`).
    /// When `install_build_tools` is true, also downloads portable CMake and Ninja if missing.
    pub fn download_cpp(
        &self,
        provider: &str,
        location: Option<&Path>,
        install_build_tools: bool,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedCppToolchain> {
        let valid = providers_for(self.platform).into_iter().any(|p| p.id == provider);
        if !valid {
            return Err(IdeError::invalid(format!(
                "C/C++ compiler provider `{provider}` is not available on {:?} {:?}",
                self.platform.os, self.platform.arch
            )));
        }
        let _guard = InstallGuard::acquire(&format!("cpp-{provider}"))?;
        let location = location.map(Path::to_path_buf).unwrap_or_else(|| self.root.join("cpp"));

        let downloaded = match provider {
            MSVC_BUILDTOOLS => self.install_msvc_buildtools(&location, cancel, emit)?,
            MINGW_WINLIBS => self.download_archive_toolchain(
                provider,
                "MinGW-w64 (GCC / WinLibs)",
                "brechtsanders/winlibs_mingw",
                WINLIBS_ASSETS,
                ("gcc", "g++"),
                &location,
                false,
                cancel,
                emit,
            )?,
            LLVM_MINGW => self.download_archive_toolchain(
                provider,
                "LLVM-MinGW (Clang + UCRT)",
                "mstorsjo/llvm-mingw",
                LLVM_MINGW_ASSETS,
                ("clang", "clang++"),
                &location,
                false,
                cancel,
                emit,
            )?,
            LLVM_WINSDK => self.download_archive_toolchain(
                provider,
                "LLVM / Clang + Windows SDK",
                "llvm/llvm-project",
                LLVM_ASSETS,
                ("clang", "clang++"),
                &location,
                true,
                cancel,
                emit,
            )?,
            LLVM_UNIX => self.download_archive_toolchain(
                provider,
                "LLVM / Clang",
                "llvm/llvm-project",
                LLVM_ASSETS,
                ("clang", "clang++"),
                &location,
                false,
                cancel,
                emit,
            )?,
            _ => unreachable!("validated above"),
        };

        if install_build_tools {
            self.ensure_cpp_build_tools(downloaded.bin_dir.as_ref(), cancel, emit)?;
        }

        Ok(downloaded)
    }

    #[allow(clippy::too_many_arguments)]
    fn download_archive_toolchain(
        &self,
        provider: &str,
        display_name: &str,
        repo: &'static str,
        assets: &[AssetRule],
        (c_cmd, cxx_cmd): (&str, &str),
        location: &Path,
        with_winsdk: bool,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedCppToolchain> {
        let http = Http::new(&self.system_path)?;
        emit(InstallEvent::Line { text: format!("Resolving the latest {display_name} release…") });
        let dl = self.resolve_release(&http, ReleaseSource::GitHub { repo }, assets, cancel)?;
        let version = dl.version.trim_start_matches("llvmorg-").to_owned();
        let dir_name = toolchain_dir_name(provider, &version);
        let target = location.join(&dir_name);
        let cxx_exe = self.platform.exe(cxx_cmd);

        let bin_dir = if let Some(existing_bin) = find_compiler_bin(&target, &cxx_exe) {
            emit(InstallEvent::Line { text: format!("{display_name} is already installed in {}", target.display()) });
            existing_bin
        } else {
            emit(InstallEvent::Line { text: format!("Downloading {} ({})", dl.name, dl.url) });
            std::fs::create_dir_all(location)?;
            let staging = location.join(format!(".staging-{dir_name}"));
            let _ = std::fs::remove_dir_all(&staging);
            std::fs::create_dir_all(&staging)?;
            let unpack_result = (|| -> IdeResult<PathBuf> {
                let archive = fetch_verified(&self.downloads, &http, &dl, &staging, cancel, emit)?;
                emit(InstallEvent::Line { text: format!("Extracting {}…", dl.name) });
                let pkg = staging.join("pkg");
                archive::extract(&archive, archive::ArchiveKind::from_name(&dl.name), &pkg, &cxx_exe, cancel)?;
                let found_bin = find_compiler_bin(&pkg, &cxx_exe).ok_or_else(|| {
                    IdeError::new(ErrorKind::Io, format!("`{cxx_exe}` was not found in {}", dl.name))
                })?;
                if cancel.is_cancelled() {
                    return Err(IdeError::cancelled());
                }
                if target.exists() {
                    std::fs::remove_dir_all(&target)?;
                }
                let root_src = unpacked_root(&pkg, &found_bin);
                std::fs::rename(&root_src, &target)?;
                clear_quarantine(&target);
                find_compiler_bin(&target, &cxx_exe).ok_or_else(|| {
                    IdeError::new(ErrorKind::Io, format!("`{cxx_exe}` was not found in {}", target.display()))
                })
            })();
            let _ = std::fs::remove_dir_all(&staging);
            unpack_result?
        };

        let mut winsysroot_path: Option<PathBuf> = None;
        let mut arch_label: Option<String> = None;
        if with_winsdk && self.platform.os == Os::Windows {
            let sysroot = target.join("winsysroot");
            let arch = winsysroot_arch(self.platform.arch);
            if inspect_winsysroot(&sysroot, arch).is_none() {
                self.download_windows_sdk_with_xwin(&http, location, &sysroot, cancel, emit)?;
            } else {
                emit(InstallEvent::Line {
                    text: format!("Standalone Windows SDK is already present in {}", sysroot.display()),
                });
            }
            emit(InstallEvent::Line {
                text: "Configuring Clang (.cfg and toolchain.cmake) to use the standalone Windows SDK…".into(),
            });
            write_llvm_winsdk_configs(&target, &bin_dir, &sysroot, arch)?;
            winsysroot_path = Some(sysroot);
            arch_label = Some(arch.to_owned());
        }

        let rel_bin = bin_dir
            .strip_prefix(&target)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| "bin".to_owned());
        let manifest = CppToolchainManifest {
            provider: provider.to_owned(),
            name: display_name.to_owned(),
            version: version.clone(),
            bin_dir: rel_bin,
            c_compiler: self.platform.exe(c_cmd),
            cxx_compiler: cxx_exe,
            winsysroot: winsysroot_path.as_ref().map(|_| "winsysroot".to_owned()),
            arch: arch_label,
        };
        std::fs::write(target.join(TOOLCHAIN_MANIFEST), serde_json::to_vec_pretty(&manifest).unwrap_or_default())?;
        emit(InstallEvent::Line { text: format!("{display_name} {version} installed in {}", target.display()) });

        Ok(DownloadedCppToolchain {
            home: target.into(),
            bin_dir: bin_dir.into(),
            provider: provider.to_owned(),
            version,
            winsysroot: winsysroot_path.map(Into::into),
        })
    }

    /// Downloads `xwin` (from `Jake-Shadle/xwin`) and uses it to fetch and unpack the Microsoft
    /// Windows SDK and MSVC CRT headers/libraries directly from Microsoft without Visual Studio.
    fn download_windows_sdk_with_xwin(
        &self,
        http: &Http,
        location: &Path,
        winsysroot: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<()> {
        let xwin_exe = self.ensure_xwin(http, location, cancel, emit)?;
        emit(InstallEvent::Line {
            text: "Downloading Windows SDK & MSVC CRT via xwin (without Visual Studio)…".into(),
        });
        let cache_dir = location.join(".xwin-cache");
        std::fs::create_dir_all(&cache_dir)?;
        let mut cmd = self.command(&xwin_exe);
        cmd.args([
            "--accept-license",
            "--arch",
            xwin_arch_arg(self.platform.arch),
            "--cache-dir",
        ])
        .arg(&cache_dir)
        .args(["splat", "--use-winsysroot-style", "--output"])
        .arg(winsysroot);
        let result = run_streamed(cmd, cancel, emit);
        let _ = std::fs::remove_dir_all(&cache_dir);
        result?;
        Ok(())
    }

    fn ensure_xwin(
        &self,
        http: &Http,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<PathBuf> {
        let xwin_dir = location.join(".xwin");
        let exe_name = self.platform.exe("xwin");
        if let Some(found) = archive::locate(&xwin_dir, std::slice::from_ref(&exe_name)) {
            return Ok(found);
        }
        emit(InstallEvent::Line { text: "Resolving xwin (standalone Windows SDK downloader)…".into() });
        let dl = self.resolve_release(http, ReleaseSource::GitHub { repo: "Jake-Shadle/xwin" }, XWIN_ASSETS, cancel)?;
        emit(InstallEvent::Line { text: format!("Downloading {} ({})", dl.name, dl.url) });
        std::fs::create_dir_all(location)?;
        let staging = location.join(".staging-xwin");
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        let result = (|| -> IdeResult<PathBuf> {
            let archive = fetch_verified(&self.downloads, http, &dl, &staging, cancel, emit)?;
            emit(InstallEvent::Line { text: "Extracting xwin…".into() });
            let pkg = staging.join("pkg");
            archive::extract(&archive, archive::ArchiveKind::from_name(&dl.name), &pkg, &exe_name, cancel)?;
            if xwin_dir.exists() {
                std::fs::remove_dir_all(&xwin_dir)?;
            }
            std::fs::rename(&pkg, &xwin_dir)?;
            clear_quarantine(&xwin_dir);
            archive::locate(&xwin_dir, std::slice::from_ref(&exe_name))
                .ok_or_else(|| IdeError::new(ErrorKind::Io, format!("`{exe_name}` was not found in {}", dl.name)))
        })();
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    fn install_msvc_buildtools(
        &self,
        location: &Path,
        cancel: &CancellationToken,
        emit: Emit,
    ) -> IdeResult<DownloadedCppToolchain> {
        if self.platform.os != Os::Windows {
            return Err(IdeError::unavailable("Visual Studio Build Tools is only available on Windows"));
        }
        if let Some(vs_root) = locate_vs_installation()
            && let Some((bin_dir, version)) = find_msvc_bin(&vs_root, self.platform.arch)
        {
            emit(InstallEvent::Line {
                text: format!("Visual Studio C++ Build Tools {version} is already installed in {}", vs_root.display()),
            });
            return Ok(DownloadedCppToolchain {
                home: vs_root.into(),
                bin_dir: bin_dir.into(),
                provider: MSVC_BUILDTOOLS.into(),
                version,
                winsysroot: None,
            });
        }
        let http = Http::new(&self.system_path)?;
        std::fs::create_dir_all(location)?;
        let staging = location.join(".staging-vs-buildtools");
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)?;
        let installer = staging.join("vs_buildtools.exe");
        let result = (|| -> IdeResult<DownloadedCppToolchain> {
            emit(InstallEvent::Line { text: format!("Downloading Visual Studio Build Tools ({VS_BUILDTOOLS_URL})…") });
            http.download(VS_BUILDTOOLS_URL, &installer, None, cancel, emit)?;
            emit(InstallEvent::Line {
                text: "Launching Visual Studio Build Tools installer (C++ workload + Windows SDK)…".into(),
            });
            // `vs_buildtools.exe` requires UAC elevation, which `CreateProcess` rejects with
            // ERROR_ELEVATION_REQUIRED (740); `Start-Process -Verb RunAs -Wait` invokes ShellExecute.
            // Exit code 3010 means installation succeeded and a reboot is recommended.
            let quoted_installer = installer.to_string_lossy().replace('\'', "''");
            let script = format!(
                "$p = Start-Process -FilePath '{quoted_installer}' -ArgumentList '--passive','--wait','--norestart','--add','Microsoft.VisualStudio.Workload.VCTools','--includeRecommended' -Verb RunAs -Wait -PassThru; \
                 if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) {{ exit $p.ExitCode }}"
            );
            let mut cmd = Command::new("powershell");
            cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
            run_streamed(cmd, cancel, emit)?;
            let vs_root = locate_vs_installation().ok_or_else(|| {
                IdeError::new(
                    ErrorKind::Io,
                    "Visual Studio Build Tools finished, but no C++ toolchain installation was detected",
                )
            })?;
            let (bin_dir, version) = find_msvc_bin(&vs_root, self.platform.arch).ok_or_else(|| {
                IdeError::new(ErrorKind::Io, format!("cl.exe was not found in {}", vs_root.display()))
            })?;
            emit(InstallEvent::Line {
                text: format!("Visual Studio Build Tools (MSVC {version}) installed in {}", vs_root.display()),
            });
            Ok(DownloadedCppToolchain {
                home: vs_root.into(),
                bin_dir: bin_dir.into(),
                provider: MSVC_BUILDTOOLS.into(),
                version,
                winsysroot: None,
            })
        })();
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    fn ensure_cpp_build_tools(&self, toolchain_bin: &Path, cancel: &CancellationToken, emit: Emit) -> IdeResult<()> {
        for tool in ["cmake", "ninja"] {
            if cancel.is_cancelled() {
                return Err(IdeError::cancelled());
            }
            let in_toolchain = self.exe_in(toolchain_bin, tool).is_some();
            let in_managed = self.exe_in(&self.root.join(tool).join("bin"), tool).is_some();
            let on_path = self.which(tool).is_some();
            if in_toolchain || in_managed || on_path {
                continue;
            }
            emit(InstallEvent::Line { text: format!("Installing companion build tool `{tool}`…") });
            if let Err(e) = self.install(tool, &[], cancel, emit) {
                if e.kind == ErrorKind::Cancelled {
                    return Err(e);
                }
                emit(InstallEvent::Line {
                    text: format!("Warning: could not install `{tool}` automatically: {}", e.message),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::select_asset;

    const WIN_X64: Platform = Platform { os: Os::Windows, arch: Arch::X64 };
    const WIN_ARM64: Platform = Platform { os: Os::Windows, arch: Arch::Arm64 };
    const LINUX_X64: Platform = Platform { os: Os::Linux, arch: Arch::X64 };
    const MAC_ARM64: Platform = Platform { os: Os::Mac, arch: Arch::Arm64 };

    #[test]
    fn windows_offers_llvm_winsdk_mingw_llvm_mingw_and_msvc() {
        let ids: Vec<String> = providers_for(WIN_X64).into_iter().map(|p| p.id).collect();
        assert_eq!(ids, [LLVM_WINSDK, MINGW_WINLIBS, LLVM_MINGW, MSVC_BUILDTOOLS]);
        let llvm = providers_for(WIN_X64).into_iter().find(|p| p.id == LLVM_WINSDK).unwrap();
        assert!(llvm.includes_windows_sdk && !llvm.requires_admin && llvm.recommended);
        let msvc = providers_for(WIN_X64).into_iter().find(|p| p.id == MSVC_BUILDTOOLS).unwrap();
        assert!(msvc.includes_windows_sdk && msvc.requires_admin);
        let linux_ids: Vec<String> = providers_for(LINUX_X64).into_iter().map(|p| p.id).collect();
        assert_eq!(linux_ids, [LLVM_UNIX]);
    }

    #[test]
    fn asset_rules_match_release_archives() {
        let winlibs = [
            "winlibs-x86_64-posix-seh-gcc-14.2.0-mingw-w64msvcrt-12.0.0-r2.zip",
            "winlibs-x86_64-posix-seh-gcc-14.2.0-mingw-w64ucrt-12.0.0-r2.zip",
            "winlibs-x86_64-posix-seh-gcc-14.2.0-mingw-w64ucrt-12.0.0-r2.7z",
        ];
        assert_eq!(
            select_asset(&winlibs, WINLIBS_ASSETS, WIN_X64),
            Some("winlibs-x86_64-posix-seh-gcc-14.2.0-mingw-w64ucrt-12.0.0-r2.zip")
        );

        let llvm_mingw = [
            "llvm-mingw-20250114-ucrt-x86_64.zip",
            "llvm-mingw-20250114-ucrt-aarch64.zip",
            "llvm-mingw-20250114-msvcrt-x86_64.zip",
        ];
        assert_eq!(select_asset(&llvm_mingw, LLVM_MINGW_ASSETS, WIN_X64), Some("llvm-mingw-20250114-ucrt-x86_64.zip"));
        assert_eq!(
            select_asset(&llvm_mingw, LLVM_MINGW_ASSETS, WIN_ARM64),
            Some("llvm-mingw-20250114-ucrt-aarch64.zip")
        );

        let llvm = [
            "clang+llvm-20.1.0-x86_64-pc-windows-msvc.tar.xz",
            "LLVM-20.1.0-Linux-X64.tar.xz",
            "LLVM-20.1.0-macOS-ARM64.tar.xz",
        ];
        assert_eq!(
            select_asset(&llvm, LLVM_ASSETS, WIN_X64),
            Some("clang+llvm-20.1.0-x86_64-pc-windows-msvc.tar.xz")
        );
        assert_eq!(select_asset(&llvm, LLVM_ASSETS, LINUX_X64), Some("LLVM-20.1.0-Linux-X64.tar.xz"));
        assert_eq!(select_asset(&llvm, LLVM_ASSETS, MAC_ARM64), Some("LLVM-20.1.0-macOS-ARM64.tar.xz"));
    }

    #[test]
    fn winsysroot_inspection_and_clang_cfg_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("llvm-winsdk-20.1.0");
        let bin = root.join("bin");
        let sysroot = root.join("winsysroot");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(sysroot.join("VC/Tools/MSVC/14.42.34433/include")).unwrap();
        std::fs::create_dir_all(sysroot.join("VC/Tools/MSVC/14.42.34433/lib/x64")).unwrap();
        for sub in ["ucrt", "um", "shared"] {
            std::fs::create_dir_all(sysroot.join("Windows Kits/10/Include/10.0.22621.0").join(sub)).unwrap();
        }
        for sub in ["ucrt", "um"] {
            std::fs::create_dir_all(sysroot.join("Windows Kits/10/Lib/10.0.22621.0").join(sub).join("x64")).unwrap();
        }

        let layout = inspect_winsysroot(&sysroot, "x64").unwrap();
        assert_eq!(layout.msvc_version, "14.42.34433");
        assert_eq!(layout.sdk_version, "10.0.22621.0");
        assert_eq!(layout.include_dirs.len(), 4);
        assert_eq!(layout.lib_dirs.len(), 3);

        write_llvm_winsdk_configs(&root, &bin, &sysroot, "x64").unwrap();
        let cl_cfg = std::fs::read_to_string(bin.join("clang-cl.cfg")).unwrap();
        assert!(cl_cfg.contains("/winsysroot"));
        assert!(cl_cfg.contains("<CFGDIR>/../winsysroot"));

        let cxx_cfg = std::fs::read_to_string(bin.join("clang++.cfg")).unwrap();
        assert!(cxx_cfg.contains("-fuse-ld=lld"));
        assert!(cxx_cfg.contains("<CFGDIR>/../winsysroot/VC/Tools/MSVC/14.42.34433/include"));
        assert!(cxx_cfg.contains("<CFGDIR>/../winsysroot/Windows Kits/10/Include/10.0.22621.0/ucrt"));
        assert!(cxx_cfg.contains("<CFGDIR>/../winsysroot/Windows Kits/10/Lib/10.0.22621.0/um/x64"));

        let cmake = std::fs::read_to_string(root.join("toolchain.cmake")).unwrap();
        assert!(cmake.contains("CMAKE_CXX_COMPILER"));
        assert!(cmake.contains("x86_64-pc-windows-msvc"));
    }

    #[test]
    fn toolchain_dir_names_strip_prefixes() {
        assert_eq!(toolchain_dir_name(LLVM_WINSDK, "llvmorg-20.1.0"), "llvm-winsdk-20.1.0");
        assert_eq!(toolchain_dir_name(MINGW_WINLIBS, "14.2.0-19.1.7-12.0.0-ucrt-r2"), "mingw-winlibs-14.2.0-19.1.7-12.0.0-ucrt-r2");
    }
}
