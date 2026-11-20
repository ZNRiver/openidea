/**
 * Download SDK / Toolchain dialog: downloads and configures language runtimes and SDKs
 * (Python/uv, Node.js, Rust, Go, Zig, .NET, PHP + Composer, Dart, Flutter, Maven, Gradle,
 * Ruby, Haskell) and adds them to the user's environment variables automatically.
 */
import { useEffect, useId, useMemo, useRef, useState } from "react";
import { choosePath } from "@/platform/choosePath";
import { tx } from "@/platform/i18n";
import {
  call,
  commands,
  isCancelled,
  isTauri,
  type DownloadedToolchain,
  type ToolchainProvider,
} from "@/platform/ipc";
import { downloadToolchain, toolchainDownloadProviders, type ToolchainDownload } from "@/platform/toolchain";
import { Dialog, IconButton, Input, ProgressBar, Select, type SelectOption, T } from "@/ui";
import styles from "./DownloadCppDialog.module.css";

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

const RUNTIME_LABELS: Record<string, string> = {
  python: "Python (uv)",
  node: "Node.js / JavaScript",
  rust: "Rust",
  go: "Go",
  zig: "Zig",
  dotnet: ".NET (C# / F#)",
  php: "PHP & Composer",
  dart: "Dart",
  flutter: "Flutter",
  "java-build": "Maven & Gradle",
  ruby: "Ruby",
  haskell: "Haskell",
};

const SYSTEM_MANAGED_PROVIDERS = new Set(["rustup-stable", "rustup-nightly", "ghcup"]);

export function DownloadToolchainDialog({
  runtime,
  onClose,
  onDownloaded,
}: {
  /** Optional runtime filter (`python`, `node`, `rust`, `go`, `zig`, `dotnet`, `php`, …). */
  runtime?: string;
  onClose: () => void;
  onDownloaded: (downloaded: DownloadedToolchain) => void;
}) {
  const [providers, setProviders] = useState<ToolchainProvider[]>([]);
  const [activeRuntime, setActiveRuntime] = useState<string>(runtime ?? "");
  const [selected, setSelected] = useState<string>("");
  const [location, setLocation] = useState("");
  const [job, setJob] = useState<ToolchainDownload | null>(null);
  const [progress, setProgress] = useState<{ fraction: number | null; message: string } | null>(null);
  const [lines, setLines] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const logRef = useRef<HTMLPreElement>(null);
  const locationId = useId();

  useEffect(() => {
    if (!isTauri()) return;
    let live = true;
    toolchainDownloadProviders(runtime ?? null)
      .then((list) => {
        if (!live) return;
        setProviders(list);
        const rec = list.find((p) => p.recommended) ?? list[0];
        if (rec) {
          setSelected((cur) => cur || rec.id);
          if (!runtime) setActiveRuntime((cur) => cur || rec.runtime);
        }
      })
      .catch((e: unknown) => {
        if (!live) return;
        const detail = errorText(e);
        setError(tx`Cannot list SDK providers: ${detail}`);
      });
    void call(commands.toolchainDownloadLocation())
      .then((dir) => live && setLocation((cur) => cur || dir))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [runtime]);

  useEffect(() => {
    const el = logRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [lines.length]);

  const runtimeOptions = useMemo<SelectOption[]>(() => {
    const seen = new Set<string>();
    const opts: SelectOption[] = [];
    for (const p of providers) {
      if (seen.has(p.runtime)) continue;
      seen.add(p.runtime);
      opts.push({ value: p.runtime, label: RUNTIME_LABELS[p.runtime] ?? p.runtime, verbatim: true });
    }
    return opts;
  }, [providers]);

  const visibleProviders = useMemo(
    () => (activeRuntime ? providers.filter((p) => p.runtime === activeRuntime) : providers),
    [providers, activeRuntime],
  );

  useEffect(() => {
    if (visibleProviders.length > 0 && !visibleProviders.some((p) => p.id === selected)) {
      const rec = visibleProviders.find((p) => p.recommended) ?? visibleProviders[0];
      if (rec) setSelected(rec.id);
    }
  }, [visibleProviders, selected]);

  const active = providers.find((p) => p.id === selected) ?? null;
  const usesOwnHome = active ? SYSTEM_MANAGED_PROVIDERS.has(active.id) : false;

  const startDownload = () => {
    if (!selected || job) return;
    setError(null);
    setLines([]);
    setProgress({ fraction: null, message: tx("Starting download…") });
    const next = downloadToolchain(selected, usesOwnHome ? null : location.trim() || null, (e) => {
      if (e.type === "progress") {
        setProgress({ fraction: e.fraction, message: e.message });
      } else {
        setProgress((p) => ({ fraction: p?.fraction ?? null, message: e.text }));
        setLines((prev) => [...prev.slice(-200), e.text]);
      }
    });
    setJob(next);
    next.promise
      .then((downloaded) => {
        onDownloaded(downloaded);
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
      title="Download SDK / Toolchain"
      width={620}
      aria-label="Download SDK / Toolchain"
      buttons={[
        { label: "Download", role: "default", disabled: !selected || !!job, onClick: startDownload },
        { label: "Cancel", role: "cancel" },
      ]}
    >
      <div className={styles.form}>
        {!runtime && runtimeOptions.length > 1 && (
          <div className={styles.grid}>
            <span>
              <T>Language / Runtime:</T>
            </span>
            <Select
              value={activeRuntime}
              options={runtimeOptions}
              onChange={setActiveRuntime}
              disabled={!!job}
              aria-label="Language / Runtime"
            />
          </div>
        )}

        <div className={styles.providers} role="radiogroup" aria-label={tx("SDK distribution")}>
          {visibleProviders.map((p) => (
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

        {!usesOwnHome && (
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
                    title: tx("SDK / Toolchain Location"),
                    defaultPath: location || undefined,
                  });
                  if (p) setLocation(p);
                }}
              />
            </div>
          </div>
        )}

        <div className={styles.note}>
          {usesOwnHome ? (
            <T>Installs into its standard user directory and adds its binaries to your environment PATH automatically.</T>
          ) : (
            tx`Unpacks into ${dir} and adds its binaries and environment variables (PATH, etc.) automatically.`
          )}
        </div>

        {(progress || lines.length > 0) && (
          <div className={styles.progress}>
            {progress && (
              <>
                <ProgressBar value={progress.fraction} aria-label={tx("Downloading SDK / Toolchain")} />
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
