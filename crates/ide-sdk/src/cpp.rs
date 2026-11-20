//! C/C++ compiler toolchain discovery and environment resolution (MinGW-w64, LLVM/Clang,
//! standalone Windows SDK via `xwin`, LLVM-MinGW, Visual Studio Build Tools / MSVC, system GCC/Clang).

use crate::discover::{par_map, subdirs};
use crate::exec::{executables_in, query, search_path, which};
use crate::{Sdk, exe};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const VERSION_TIMEOUT: Duration = Duration::from_secs(4);
const TOOLCHAIN_MANIFEST: &str = "openidea-toolchain.json";

/// Folders the IDE downloads C/C++ toolchains into ([`add_cpp_root`]).
static EXTRA_ROOTS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Registers a folder whose subdirectories are scanned for downloaded C/C++ toolchains.
pub fn add_cpp_root(dir: &Path) {
    let mut roots = EXTRA_ROOTS.lock().unwrap_or_else(|e| e.into_inner());
    if !roots.iter().any(|r| r == dir) {
        roots.push(dir.to_path_buf());
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolchainManifest {
    provider: String,
    name: String,
    version: String,
    bin_dir: String,
    c_compiler: String,
    cxx_compiler: String,
    #[serde(default)]
    winsysroot: Option<String>,
    #[serde(default)]
    arch: Option<String>,
}

/// Resolved environment for a C/C++ toolchain applied to CMake, `clangd`, and run configurations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CppToolchainEnv {
    pub home: PathBuf,
    pub bin_dir: PathBuf,
    pub c_compiler: Option<PathBuf>,
    pub cxx_compiler: Option<PathBuf>,
    pub winsysroot: Option<PathBuf>,
    pub include_dirs: Vec<PathBuf>,
    pub lib_dirs: Vec<PathBuf>,
    pub is_msvc: bool,
}

fn read_manifest(dir: &Path) -> Option<ToolchainManifest> {
    let bytes = std::fs::read(dir.join(TOOLCHAIN_MANIFEST)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn current_win_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" }
}

/// Resolves include and library directories inside a `xwin --use-winsysroot-style` folder.
fn winsysroot_dirs(winsysroot: &Path, arch: &str) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let newest_dir = |parent: &Path| -> Option<PathBuf> {
        let mut dirs: Vec<(String, PathBuf)> = std::fs::read_dir(parent)
            .ok()?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
            .collect();
        dirs.sort_by(|a, b| ide_core::tool_path::version_key(&b.0).cmp(&ide_core::tool_path::version_key(&a.0)));
        dirs.into_iter().next().map(|(_, p)| p)
    };
    let mut includes = Vec::new();
    let mut libs = Vec::new();
    if let Some(msvc) = newest_dir(&winsysroot.join("VC").join("Tools").join("MSVC")) {
        let inc = msvc.join("include");
        if inc.is_dir() {
            includes.push(inc);
        }
        let lib = msvc.join("lib").join(arch);
        if lib.is_dir() {
            libs.push(lib);
        }
    }
    let sdk_base = winsysroot.join("Windows Kits").join("10");
    if let Some(sdk_inc) = newest_dir(&sdk_base.join("Include")) {
        let ver = sdk_inc.file_name().map(|n| n.to_string_lossy().into_owned());
        for sub in ["ucrt", "um", "shared", "winrt", "cppwinrt"] {
            let dir = sdk_inc.join(sub);
            if dir.is_dir() {
                includes.push(dir);
            }
        }
        if let Some(ver) = ver {
            let sdk_lib = sdk_base.join("Lib").join(ver);
            for sub in ["ucrt", "um"] {
                let dir = sdk_lib.join(sub).join(arch);
                if dir.is_dir() {
                    libs.push(dir);
                }
            }
        }
    }
    (includes, libs)
}

/// Joins directories with the platform's path-list separator (`;` on Windows, `:` on Unix).
pub(crate) fn join_search_dirs(dirs: &[PathBuf]) -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    dirs.iter().map(|d| d.to_string_lossy().into_owned()).collect::<Vec<_>>().join(sep)
}

fn find_msvc_bin(vs_root: &Path) -> Option<(PathBuf, String)> {
    let msvc_base = vs_root.join("VC").join("Tools").join("MSVC");
    let mut versions: Vec<(String, PathBuf)> = std::fs::read_dir(&msvc_base)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    versions.sort_by(|a, b| ide_core::tool_path::version_key(&b.0).cmp(&ide_core::tool_path::version_key(&a.0)));
    let subdirs_order: &[&str] = if cfg!(target_arch = "aarch64") {
        &["Hostarm64/arm64", "Hostx64/arm64", "Hostx64/x64"]
    } else {
        &["Hostx64/x64", "Hostx86/x64", "Hostx86/x86"]
    };
    for (version, dir) in versions {
        for sub in subdirs_order {
            let bin = dir.join("bin").join(sub);
            if bin.join("cl.exe").is_file() {
                return Some((bin, version));
            }
        }
    }
    None
}

fn locate_vs_roots() -> Vec<PathBuf> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let Some(pf86) = std::env::var_os("ProgramFiles(x86)").map(PathBuf::from) else {
        return Vec::new();
    };
    let vswhere = pf86.join("Microsoft Visual Studio").join("Installer").join("vswhere.exe");
    if !vswhere.is_file() {
        return Vec::new();
    }
    let Some(out) = query(
        &vswhere,
        &["-products", "*", "-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64", "-property", "installationPath"],
        VERSION_TIMEOUT,
    ) else {
        return Vec::new();
    };
    out.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).filter(|p| p.is_dir()).collect()
}

fn parse_compiler_version(output: &str) -> Option<String> {
    for token in output.split(|c: char| c.is_whitespace() || matches!(c, ',' | '(' | ')' | '"' | '\'' | ';')) {
        let t = token.trim_start_matches(['v', 'V']).trim_end_matches(['.', ':']);
        if t.starts_with(|c: char| c.is_ascii_digit())
            && t.contains('.')
            && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
            && t.chars().filter(|c| c.is_ascii_digit()).count() >= 2
        {
            return Some(t.to_owned());
        }
    }
    None
}

/// On macOS, `/usr/bin/clang++` is a stub that pops up an installer dialog when
/// Xcode / Command Line Tools are not installed.
fn usable_compiler(path: &Path) -> bool {
    if cfg!(target_os = "macos") && path.starts_with("/usr/bin") {
        return Path::new("/Library/Developer/CommandLineTools/usr/bin").is_dir()
            || Path::new("/Applications/Xcode.app").is_dir();
    }
    true
}

/// Resolves compiler executables and standalone Windows SDK paths for a toolchain directory
/// (either a toolchain root, its `bin/` directory, or a Visual Studio installation root).
pub fn toolchain_env(path: &Path) -> Option<CppToolchainEnv> {
    if let Some(manifest) = read_manifest(path) {
        let bin_dir = path.join(&manifest.bin_dir);
        let c_compiler = Some(bin_dir.join(&manifest.c_compiler)).filter(|p| p.is_file());
        let cxx_compiler = Some(bin_dir.join(&manifest.cxx_compiler)).filter(|p| p.is_file());
        if c_compiler.is_none() && cxx_compiler.is_none() {
            return None;
        }
        let winsysroot = manifest.winsysroot.map(|rel| path.join(rel)).filter(|p| p.is_dir());
        let arch = manifest.arch.as_deref().unwrap_or(current_win_arch());
        let (include_dirs, lib_dirs) =
            winsysroot.as_deref().map(|s| winsysroot_dirs(s, arch)).unwrap_or_default();
        return Some(CppToolchainEnv {
            home: path.to_path_buf(),
            bin_dir,
            c_compiler,
            cxx_compiler,
            winsysroot,
            include_dirs,
            lib_dirs,
            is_msvc: manifest.provider == "msvc-buildtools",
        });
    }

    if let Some((bin_dir, _)) = find_msvc_bin(path) {
        let cl = bin_dir.join("cl.exe");
        return Some(CppToolchainEnv {
            home: path.to_path_buf(),
            bin_dir,
            c_compiler: Some(cl.clone()),
            cxx_compiler: Some(cl),
            winsysroot: None,
            include_dirs: Vec::new(),
            lib_dirs: Vec::new(),
            is_msvc: true,
        });
    }

    let bin_dir = if path.join("bin").is_dir()
        && [exe("clang++"), exe("g++"), exe("clang-cl"), exe("cl"), exe("c++")]
            .iter()
            .any(|name| path.join("bin").join(name).is_file())
    {
        path.join("bin")
    } else if path.is_file() {
        path.parent()?.to_path_buf()
    } else {
        path.to_path_buf()
    };

    let home = if bin_dir.file_name().is_some_and(|n| n.eq_ignore_ascii_case("bin")) {
        bin_dir.parent().unwrap_or(&bin_dir).to_path_buf()
    } else {
        bin_dir.clone()
    };

    let pick = |names: &[&str]| names.iter().map(|n| bin_dir.join(exe(n))).find(|p| p.is_file());
    let cxx_compiler = pick(&["clang++", "g++", "clang-cl", "cl", "c++"]);
    let c_compiler = pick(&["clang", "gcc", "clang-cl", "cl", "cc"]);
    if cxx_compiler.is_none() && c_compiler.is_none() {
        return None;
    }

    let is_msvc = cxx_compiler
        .as_ref()
        .and_then(|p| p.file_stem())
        .is_some_and(|s| s.eq_ignore_ascii_case("cl"));
    let winsysroot = Some(home.join("winsysroot")).filter(|p| p.is_dir());
    let (include_dirs, lib_dirs) =
        winsysroot.as_deref().map(|s| winsysroot_dirs(s, current_win_arch())).unwrap_or_default();

    Some(CppToolchainEnv {
        home,
        bin_dir,
        c_compiler,
        cxx_compiler,
        winsysroot,
        include_dirs,
        lib_dirs,
        is_msvc,
    })
}

/// First IDE-downloaded C/C++ toolchain environment, used as a fallback when no compiler
/// is configured and none is on `PATH`.
pub fn default_downloaded_env() -> Option<CppToolchainEnv> {
    let roots = EXTRA_ROOTS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for root in roots {
        for dir in subdirs(&root) {
            if dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
                continue;
            }
            if let Some(env) = toolchain_env(&dir) {
                return Some(env);
            }
        }
    }
    None
}

/// Whether any C/C++ compiler is found on `PATH`.
pub(crate) fn has_system_compiler() -> bool {
    ["clang++", "g++", "clang-cl", "cl", "c++"]
        .into_iter()
        .any(|name| which(name).is_some_and(|p| usable_compiler(&p)))
}

/// Resolves a single toolchain folder (or Visual Studio install root) into an [`Sdk`] entry.
pub fn sdk_at(path: &Path) -> Option<Sdk> {
    sdk_at_with_source(path, "downloaded")
}

fn sdk_at_with_source(path: &Path, source: &str) -> Option<Sdk> {
    if let Some(manifest) = read_manifest(path) {
        let bin = path.join(&manifest.bin_dir);
        let cxx = bin.join(&manifest.cxx_compiler);
        if !cxx.is_file() {
            return None;
        }
        let winsysroot = manifest.winsysroot.map(|rel| path.join(rel)).filter(|p| p.is_dir());
        return Some(Sdk {
            path: path.to_path_buf().into(),
            name: format!("{} {}", manifest.name, manifest.version).trim().to_owned(),
            version: Some(manifest.version),
            source: source.to_owned(),
            home: Some(path.to_path_buf().into()),
            extra: winsysroot.map(Into::into),
        });
    }

    if let Some((_, version)) = find_msvc_bin(path) {
        let edition = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Visual Studio".into());
        return Some(Sdk {
            path: path.to_path_buf().into(),
            name: format!("MSVC {version} ({edition})"),
            version: Some(version),
            source: if source == "downloaded" { "vs".to_owned() } else { source.to_owned() },
            home: Some(path.to_path_buf().into()),
            extra: None,
        });
    }

    let env = toolchain_env(path)?;
    let compiler = env.cxx_compiler.as_ref().or(env.c_compiler.as_ref())?;
    if !usable_compiler(compiler) {
        return None;
    }
    let stem = compiler.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    let version = if stem == "cl" {
        None
    } else {
        query(compiler, &["--version"], VERSION_TIMEOUT).and_then(|out| parse_compiler_version(&out))
    };
    let family = if stem.contains("clang") {
        if env.winsysroot.is_some() { "LLVM / Clang + Windows SDK" } else { "Clang" }
    } else if stem == "g++" || stem == "gcc" {
        if cfg!(windows) { "MinGW GCC" } else { "GCC" }
    } else if stem == "cl" {
        "MSVC"
    } else {
        "C/C++"
    };
    let name = match &version {
        Some(v) => format!("{family} {v}"),
        None => family.to_owned(),
    };
    Some(Sdk {
        path: env.home.clone().into(),
        name,
        version,
        source: source.to_owned(),
        home: Some(env.home.into()),
        extra: env.winsysroot.map(Into::into),
    })
}

/// Detects installed C/C++ compiler toolchains: IDE downloads first, then Visual Studio,
/// standard MinGW / MSYS2 / LLVM directories, and compilers on `PATH`.
pub fn detect() -> Vec<Sdk> {
    let mut candidates: Vec<(PathBuf, &'static str)> = Vec::new();

    let extra = EXTRA_ROOTS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for root in extra {
        for dir in subdirs(&root) {
            if dir.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
                continue;
            }
            candidates.push((dir, "downloaded"));
        }
    }

    if cfg!(windows) {
        for vs in locate_vs_roots() {
            candidates.push((vs, "vs"));
        }
        for (dir, source) in [
            (r"C:\msys64\ucrt64", "msys2"),
            (r"C:\msys64\mingw64", "msys2"),
            (r"C:\msys64\clang64", "msys2"),
            (r"C:\mingw64", "mingw"),
            (r"C:\Program Files\LLVM", "llvm"),
        ] {
            let p = PathBuf::from(dir);
            if p.is_dir() {
                candidates.push((p, source));
            }
        }
    } else {
        for (dir, source) in [("/opt/homebrew/opt/llvm", "homebrew"), ("/usr/local/opt/llvm", "homebrew")] {
            let p = PathBuf::from(dir);
            if p.is_dir() {
                candidates.push((p, source));
            }
        }
    }

    let path_dirs: Vec<PathBuf> = std::env::split_paths(&search_path()).collect();
    for tool in ["clang++", "g++", "clang-cl", "cl"] {
        for bin_exe in executables_in(tool, path_dirs.iter().cloned()) {
            if !usable_compiler(&bin_exe) {
                continue;
            }
            if let Some(bin_dir) = bin_exe.parent() {
                let root = if bin_dir.file_name().is_some_and(|n| n.eq_ignore_ascii_case("bin")) {
                    bin_dir.parent().unwrap_or(bin_dir).to_path_buf()
                } else {
                    bin_dir.to_path_buf()
                };
                candidates.push((root, "path"));
            }
        }
    }

    let mut seen = HashSet::new();
    let deduped: Vec<(PathBuf, &'static str)> = candidates
        .into_iter()
        .filter(|(p, _)| p.is_dir() && seen.insert(dunce::canonicalize(p).unwrap_or_else(|_| p.clone())))
        .collect();

    par_map(&deduped, |(dir, source)| sdk_at_with_source(dir, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_toolchain_resolves_winsysroot_includes_and_libs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("llvm-winsdk-20.1.0");
        let bin = root.join("bin");
        let sysroot = root.join("winsysroot");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join(exe("clang")), "").unwrap();
        std::fs::write(bin.join(exe("clang++")), "").unwrap();
        std::fs::create_dir_all(sysroot.join("VC/Tools/MSVC/14.42.34433/include")).unwrap();
        std::fs::create_dir_all(sysroot.join("VC/Tools/MSVC/14.42.34433/lib/x64")).unwrap();
        std::fs::create_dir_all(sysroot.join("Windows Kits/10/Include/10.0.22621.0/ucrt")).unwrap();
        std::fs::create_dir_all(sysroot.join("Windows Kits/10/Lib/10.0.22621.0/ucrt/x64")).unwrap();
        std::fs::write(
            root.join(TOOLCHAIN_MANIFEST),
            serde_json::json!({
                "provider": "llvm-winsdk",
                "name": "LLVM / Clang + Windows SDK",
                "version": "20.1.0",
                "binDir": "bin",
                "cCompiler": exe("clang"),
                "cxxCompiler": exe("clang++"),
                "winsysroot": "winsysroot",
                "arch": "x64",
            })
            .to_string(),
        )
        .unwrap();

        let env = toolchain_env(&root).unwrap();
        assert_eq!(env.bin_dir, bin);
        assert_eq!(env.winsysroot, Some(sysroot));
        assert_eq!(env.include_dirs.len(), 2);
        assert_eq!(env.lib_dirs.len(), 2);
        assert!(!env.is_msvc);

        let sdk = sdk_at(&root).unwrap();
        assert_eq!(sdk.name, "LLVM / Clang + Windows SDK 20.1.0");
        assert_eq!(sdk.source, "downloaded");
        assert!(sdk.extra.is_some());
    }
}
