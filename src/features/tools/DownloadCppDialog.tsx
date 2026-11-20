/**
 * Download C/C++ Compiler dialog: offers MinGW-w64 (GCC / WinLibs), LLVM/Clang +
 * standalone Windows SDK (via xwin, without Visual Studio), LLVM-MinGW, or Visual
 * Studio Build Tools (MSVC), plus optional CMake & Ninja installation.
 */
import { useEffect, useId, useRef, useState } from "react";
import { choosePath } from "@/platform/choosePath";
import { cppDownloadProviders, downloadCpp, type CppDownload } from "@/platform/cpp";
import { tx } from "@/platform/i18n";
import { call, commands, isCancelled, isTauri, type CppProvider, type Sdk } from "@/platform/ipc";
import { Checkbox, Dialog, IconButton, Input, ProgressBar, T } from "@/ui";
import styles from "./DownloadCppDialog.module.css";

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function DownloadCppDialog({
  onClose,
  onDownloaded,
}: {
  onClose: () => void;
  onDownloaded: (sdk: Sdk) => void;
}) {
  const [providers, setProviders] = useState<CppProvider[]>([]);
  const [selected, setSelected] = useState<string>("");
  const [location, setLocation] = useState("");
  const [installBuildTools, setInstallBuildTools] = useState(true);
  const [job, setJob] = useState<CppDownload | null>(null);
  const [progress, setProgress] = useState<{ fraction: number | null; message: string } | null>(null);
  const [lines, setLines] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const logRef = useRef<HTMLPreElement>(null);
  const locationId = useId();

  useEffect(() => {
    if (!isTauri()) return;
    let live = true;
    cppDownloadProviders()
      .then((list) => {
        if (!live) return;
        setProviders(list);
        const rec = list.find((p) => p.recommended) ?? list[0];
        if (rec) setSelected((cur) => cur || rec.id);
      })
      .catch((e: unknown) => {
        if (!live) return;
        const detail = errorText(e);
        setError(tx`Cannot list C/C++ compiler providers: ${detail}`);
      });
    void call(commands.cppDownloadLocation())
      .then((dir) => live && setLocation((cur) => cur || dir))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, []);

  useEffect(() => {
    const el = logRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [lines.length]);

  const active = providers.find((p) => p.id === selected) ?? null;
  const isSystemInstaller = active?.id === "msvc-buildtools";

  const startDownload = () => {
    if (!selected || job) return;
    setError(null);
    setLines([]);
    setProgress({ fraction: null, message: tx("Starting download…") });
    const next = downloadCpp(selected, isSystemInstaller ? null : location.trim() || null, installBuildTools, (e) => {
      if (e.type === "progress") {
        setProgress({ fraction: e.fraction, message: e.message });
      } else {
        setProgress((p) => ({ fraction: p?.fraction ?? null, message: e.text }));
        setLines((prev) => [...prev.slice(-200), e.text]);
      }
    });
    setJob(next);
    next.promise
      .then((sdk) => {
        onDownloaded(sdk);
      })
      .catch((e: unknown) => {
        if (!isCancelled(e)) setError(errorText(e));
        setProgress(null);
      })
      .finally(() => setJob(null));
  };

  const dir = location || "…";

  return (
    <Dialog
      open
      onClose={() => (job ? job.cancel() : onClose())}
      title="Download C/C++ Compiler"
      width={620}
      aria-label="Download C/C++ Compiler"
      buttons={[
        { label: "Download", role: "default", disabled: !selected || !!job, onClick: startDownload },
        { label: "Cancel", role: "cancel" },
      ]}
    >
      <div className={styles.form}>
        <div className={styles.providers} role="radiogroup" aria-label={tx("Compiler distribution")}>
          {providers.map((p) => (
            <button
              key={p.id}
              type="button"
              role="radio"
              aria-checked={selected === p.id}
              className={styles.providerCard}
              disabled={!!job}
              onClick={() => setSelected(p.id)}
            >
              <div className={styles.providerHeader}>
                <span className={styles.providerName}>{p.name}</span>
                {p.recommended && (
                  <span className={`${styles.badge} ${styles.badgeAccent}`}>
                    <T>Recommended</T>
                  </span>
                )}
                {p.includesWindowsSdk && !p.requiresAdmin && (
                  <span className={styles.badge}>
                    <T>Includes Windows SDK (no Visual Studio)</T>
                  </span>
                )}
                {p.requiresAdmin && (
                  <span className={styles.badge}>
                    <T>Requires admin</T>
                  </span>
                )}
              </div>
              <div className={styles.providerDesc}>{p.description}</div>
            </button>
          ))}
        </div>

        {!isSystemInstaller && (
          <div className={styles.grid}>
            <label htmlFor={locationId}>
              <T>Location:</T>
            </label>
            <div className={styles.pathRow}>
              <Input
                id={locationId}
                mono
                className={styles.pathInput}
                value={location}
                disabled={!!job}
                onChange={(e) => setLocation(e.target.value)}
              />
              <IconButton
                icon="folder-open"
                size="small"
                title="Browse…"
                disabled={!!job}
                onClick={async () => {
                  const p = await choosePath({
                    directory: true,
                    title: tx("C/C++ Toolchain Location"),
                    defaultPath: location || undefined,
                  });
                  if (p) setLocation(p);
                }}
              />
            </div>
          </div>
        )}

        <Checkbox
          checked={installBuildTools}
          onChange={setInstallBuildTools}
          disabled={!!job}
          label="Also download CMake & Ninja if missing"
        />

        <div className={styles.note}>
          {isSystemInstaller ? (
            <T>
              Visual Studio Build Tools installs system-wide using Microsoft's official installer and prompts for
              administrator approval.
            </T>
          ) : (
            tx`The toolchain is unpacked into ${dir}, where CMake and clangd detect it automatically.`
          )}
        </div>

        {(progress || lines.length > 0) && (
          <div className={styles.progress}>
            {progress && (
              <>
                <ProgressBar value={progress.fraction} aria-label={tx("Downloading C/C++ compiler")} />
                <span className={styles.note}>{progress.message}</span>
              </>
            )}
            {lines.length > 0 && (
              <pre ref={logRef} className={styles.log} aria-label={tx("Output")} tabIndex={0}>
                {lines.join("\n")}
              </pre>
            )}
          </div>
        )}

        {error && (
          <div className={styles.error} role="alert">
            {error}
          </div>
        )}
      </div>
    </Dialog>
  );
}
