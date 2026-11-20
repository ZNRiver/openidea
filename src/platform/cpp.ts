/**
 * Download C/C++ Compiler (`cpp_download*` commands), shared by New Project,
 * Settings › Languages & Frameworks › C/C++, CMake settings, and missing-compiler prompts.
 * The overlay dialog is opened via the `cpp.downloadCompiler` action (features/tools).
 */
import { Channel } from "@tauri-apps/api/core";
import { executeAction, getAction } from "./actions";
import { call, commands, nextRequestId, type CppProvider, type InstallEvent, type Sdk } from "./ipc";

export const DOWNLOAD_CPP_ACTION = "cpp.downloadCompiler";

export interface DownloadCppArgs {
  onComplete?: (sdk: Sdk | null) => void;
}

export interface CppDownload {
  promise: Promise<Sdk>;
  cancel: () => void;
}

/** Downloads a C/C++ compiler toolchain into `location` (null: the IDE's `tools/cpp` folder). */
export function downloadCpp(
  provider: string,
  location: string | null,
  installBuildTools: boolean,
  onEvent: (e: InstallEvent) => void,
): CppDownload {
  const request = nextRequestId();
  const channel = new Channel<InstallEvent>();
  channel.onmessage = onEvent;
  return {
    promise: call(commands.cppDownload(provider, location, installBuildTools, request, channel)),
    cancel: () => void commands.cancelRequest(request).catch(() => {}),
  };
}

/** Fetches the C/C++ compiler providers offered for this platform. */
export function cppDownloadProviders(): Promise<CppProvider[]> {
  return commands.cppDownloadProviders();
}

/** Opens the Download C/C++ Compiler dialog and resolves with the downloaded SDK, or null if cancelled. */
export function openDownloadCppDialog(): Promise<Sdk | null> {
  if (!getAction(DOWNLOAD_CPP_ACTION)) return Promise.resolve(null);
  return new Promise<Sdk | null>((resolve) => {
    const args: DownloadCppArgs = { onComplete: resolve };
    void executeAction(DOWNLOAD_CPP_ACTION, args).catch(() => resolve(null));
  });
}
