/**
 * `cpp.downloadCompiler`: opens the Download C/C++ Compiler overlay dialog and prompts
 * when a CMake/C++ project is opened without any installed C/C++ compiler.
 */
import { create } from "zustand";
import { executeAction, registerActions } from "@/platform/actions";
import { DOWNLOAD_CPP_ACTION, type DownloadCppArgs } from "@/platform/cpp";
import { tx } from "@/platform/i18n";
import { call, commands, isTauri, type Sdk } from "@/platform/ipc";
import { useProject } from "@/platform/project";
import { notifications } from "@/workbench/contributions";
import { DownloadCppDialog } from "./DownloadCppDialog";

const CPP_SETTINGS = "languages.cpp";

interface DownloadCppState {
  open: boolean;
  onComplete: ((sdk: Sdk | null) => void) | null;
}

const useDownloadCpp = create<DownloadCppState>(() => ({ open: false, onComplete: null }));

function openDialog(args?: DownloadCppArgs) {
  const prev = useDownloadCpp.getState();
  if (prev.open) {
    args?.onComplete?.(null);
    return;
  }
  useDownloadCpp.setState({ open: true, onComplete: args?.onComplete ?? null });
}

function closeDialog(sdk: Sdk | null) {
  const cb = useDownloadCpp.getState().onComplete;
  useDownloadCpp.setState({ open: false, onComplete: null });
  cb?.(sdk);
}

export function DownloadCppOverlay() {
  const open = useDownloadCpp((s) => s.open);
  if (!open) return null;
  return (
    <DownloadCppDialog
      onClose={() => closeDialog(null)}
      onDownloaded={(sdk) => {
        closeDialog(sdk);
        const { name, path } = sdk;
        notifications().notify({
          group: "tools",
          level: "info",
          key: "cpp-compiler-downloaded",
          title: tx("C/C++ compiler installed"),
          body: tx`${name} is ready in ${path}.`,
        });
      }}
    />
  );
}

function isDownloadCppArgs(arg: unknown): arg is DownloadCppArgs {
  return !!arg && typeof arg === "object" && "onComplete" in arg;
}

export function registerCppActions(): () => void {
  return registerActions([
    {
      id: DOWNLOAD_CPP_ACTION,
      title: "Download C/C++ Compiler…",
      category: "Tools",
      icon: "package",
      keywords: ["c++", "cpp", "compiler", "mingw", "llvm", "clang", "msvc", "visual studio", "windows sdk"],
      run: ({ arg }) => {
        openDialog(isDownloadCppArgs(arg) ? arg : undefined);
      },
    },
  ]);
}

/**
 * When a trusted CMake / C++ project opens and no C/C++ compiler is detected,
 * offer a one-click prompt to download MinGW-w64, LLVM/Clang + Windows SDK, or Visual Studio Build Tools.
 */
export function installCppPrompts(): () => void {
  const checkedProjects = new Set<string>();

  const checkProject = async () => {
    if (!isTauri()) return;
    const state = useProject.getState();
    const project = state.project;
    if (!project || !project.trusted || checkedProjects.has(project.root)) return;
    checkedProjects.add(project.root);
    try {
      const entries = await call(commands.fsReadDir(project.root));
      const hasCppProject = entries.some(
        (e) =>
          e.name === "CMakeLists.txt" ||
          e.name.endsWith(".cpp") ||
          e.name.endsWith(".cc") ||
          e.name.endsWith(".cxx") ||
          e.name.endsWith(".c"),
      );
      if (!hasCppProject) return;
      const sdks = await call(commands.sdkDetect("cpp"));
      if (sdks.length > 0) return;
      notifications().notify({
        group: "tools",
        level: "warning",
        key: "cpp-compiler-missing",
        sticky: true,
        title: "No C/C++ compiler found",
        body: "Install MinGW-w64, LLVM/Clang with standalone Windows SDK, or Visual Studio Build Tools to build this project.",
        actions: [
          {
            label: "Download Compiler…",
            run: () => void executeAction(DOWNLOAD_CPP_ACTION),
          },
          {
            label: "C/C++ Settings…",
            run: () => void executeAction("file.settings", CPP_SETTINGS),
          },
        ],
      });
    } catch {
      // Best-effort prompt; stay silent if filesystem or SDK detection fails.
    }
  };

  void checkProject();
  return useProject.subscribe(() => {
    void checkProject();
  });
}
