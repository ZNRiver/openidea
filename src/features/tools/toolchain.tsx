/**
 * `tools.downloadToolchain`: opens the Download SDK / Toolchain overlay dialog for any
 * supported language runtime (Python/uv, Node.js, Rust, Go, Zig, .NET, PHP, Dart, Flutter, …).
 */
import { create } from "zustand";
import { registerActions } from "@/platform/actions";
import { tx } from "@/platform/i18n";
import type { DownloadedToolchain } from "@/platform/ipc";
import { DOWNLOAD_TOOLCHAIN_ACTION, type DownloadToolchainArgs } from "@/platform/toolchain";
import { notifications } from "@/workbench/contributions";
import { DownloadToolchainDialog } from "./DownloadToolchainDialog";

interface DownloadToolchainState {
  open: boolean;
  runtime?: string;
  onComplete: ((downloaded: DownloadedToolchain | null) => void) | null;
}

const useDownloadToolchain = create<DownloadToolchainState>(() => ({ open: false, onComplete: null }));

function openDialog(args?: DownloadToolchainArgs) {
  const prev = useDownloadToolchain.getState();
  if (prev.open) {
    args?.onComplete?.(null);
    return;
  }
  useDownloadToolchain.setState({ open: true, runtime: args?.runtime, onComplete: args?.onComplete ?? null });
}

function closeDialog(downloaded: DownloadedToolchain | null) {
  const cb = useDownloadToolchain.getState().onComplete;
  useDownloadToolchain.setState({ open: false, runtime: undefined, onComplete: null });
  cb?.(downloaded);
}

export function DownloadToolchainOverlay() {
  const open = useDownloadToolchain((s) => s.open);
  const runtime = useDownloadToolchain((s) => s.runtime);
  if (!open) return null;
  return (
    <DownloadToolchainDialog
      runtime={runtime}
      onClose={() => closeDialog(null)}
      onDownloaded={(downloaded) => {
        closeDialog(downloaded);
        const { name, home } = downloaded;
        notifications().notify({
          group: "tools",
          level: "info",
          key: `toolchain-downloaded-${downloaded.id}`,
          title: tx("SDK / toolchain installed"),
          body: tx`${name} is ready in ${home}.`,
        });
      }}
    />
  );
}

function isDownloadToolchainArgs(arg: unknown): arg is DownloadToolchainArgs {
  return !!arg && typeof arg === "object" && ("runtime" in arg || "onComplete" in arg);
}

export function registerToolchainActions(): () => void {
  return registerActions([
    {
      id: DOWNLOAD_TOOLCHAIN_ACTION,
      title: "Download SDK / Toolchain…",
      category: "Tools",
      icon: "package",
      keywords: [
        "sdk",
        "toolchain",
        "download",
        "python",
        "uv",
        "node",
        "nodejs",
        "pnpm",
        "bun",
        "deno",
        "rust",
        "rustup",
        "cargo",
        "go",
        "golang",
        "zig",
        "dotnet",
        "php",
        "composer",
        "dart",
        "flutter",
        "maven",
        "gradle",
        "ruby",
        "haskell",
      ],
      run: ({ arg }) => {
        openDialog(isDownloadToolchainArgs(arg) ? arg : undefined);
      },
    },
  ]);
}
