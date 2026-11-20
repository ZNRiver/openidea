/**
 * Download SDK / Toolchain (`toolchain_download*` commands), shared by New Project,
 * Settings › Languages & Frameworks, and Command Palette (`tools.downloadToolchain`).
 */
import { Channel } from "@tauri-apps/api/core";
import { executeAction, getAction } from "./actions";
import {
  call,
  commands,
  nextRequestId,
  type DownloadedToolchain,
  type InstallEvent,
  type ToolchainProvider,
} from "./ipc";

export const DOWNLOAD_TOOLCHAIN_ACTION = "tools.downloadToolchain";

export interface DownloadToolchainArgs {
  /** Optional runtime filter (`python`, `node`, `rust`, `go`, `zig`, `dotnet`, `php`, `dart`, `flutter`, `java-build`, `ruby`, `haskell`). */
  runtime?: string;
  onComplete?: (downloaded: DownloadedToolchain | null) => void;
}

export interface ToolchainDownload {
  promise: Promise<DownloadedToolchain>;
  cancel: () => void;
}

/** Downloads a language SDK / runtime into `location` (null: `<tools>/sdks`) and registers its environment variables. */
export function downloadToolchain(
  provider: string,
  location: string | null,
  onEvent: (e: InstallEvent) => void,
): ToolchainDownload {
  const request = nextRequestId();
  const channel = new Channel<InstallEvent>();
  channel.onmessage = onEvent;
  return {
    promise: call(commands.toolchainDownload(provider, location, request, channel)),
    cancel: () => void commands.cancelRequest(request).catch(() => {}),
  };
}

/** Fetches the SDK / runtime distributions offered for this platform (optionally filtered by `runtime`). */
export function toolchainDownloadProviders(runtime?: string | null): Promise<ToolchainProvider[]> {
  return commands.toolchainDownloadProviders(runtime ?? null);
}

/** Maps a missing CLI program name (e.g. `uv`, `node`, `cargo`, `go`, `zig`, `dotnet`, `php`, `composer`) to a downloadable runtime group. */
export function runtimeForProgram(program: string): string | null {
  switch (program.toLowerCase()) {
    case "uv":
    case "python":
    case "python3":
      return "python";
    case "node":
    case "npm":
    case "npx":
    case "pnpm":
    case "yarn":
    case "bun":
    case "deno":
      return "node";
    case "cargo":
    case "rustc":
    case "rustup":
      return "rust";
    case "go":
      return "go";
    case "zig":
      return "zig";
    case "dotnet":
      return "dotnet";
    case "php":
    case "composer":
      return "php";
    case "dart":
      return "dart";
    case "flutter":
      return "flutter";
    case "mvn":
    case "gradle":
      return "java-build";
    case "ruby":
    case "gem":
    case "bundle":
    case "rails":
      return "ruby";
    case "cabal":
    case "ghc":
    case "ghcup":
      return "haskell";
    default:
      return null;
  }
}

/** Opens the Download SDK / Toolchain dialog and resolves with the downloaded toolchain, or null if cancelled. */
export function openDownloadToolchainDialog(runtime?: string): Promise<DownloadedToolchain | null> {
  if (!getAction(DOWNLOAD_TOOLCHAIN_ACTION)) return Promise.resolve(null);
  return new Promise<DownloadedToolchain | null>((resolve) => {
    const args: DownloadToolchainArgs = { runtime, onComplete: resolve };
    void executeAction(DOWNLOAD_TOOLCHAIN_ACTION, args).catch(() => resolve(null));
  });
}
