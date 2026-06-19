import { useCallback, useEffect, useRef, useState } from "react";
import { check, Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { getVersion } from "@tauri-apps/api/app";

export type UpdateStatus =
  | "idle" // no check has resolved yet, or checking found nothing actionable
  | "checking" // a check is in flight
  | "available" // a newer version exists and is ready to install
  | "downloading" // the user opted in; bytes are streaming
  | "installing" // download finished, applying the update before relaunch
  | "uptodate" // a check completed and we are on the latest version
  | "error"; // the check or install failed

export interface UpdaterState {
  status: UpdateStatus;
  /** The version offered by the feed (only set when status is past "available"). */
  newVersion: string | null;
  /** The version currently running. */
  currentVersion: string;
  /** Release notes for the available update, if the feed provided any. */
  notes: string | null;
  /** Download progress 0-100 while status is "downloading". */
  progress: number | null;
  /** Human-readable error when status is "error". */
  error: string | null;
  /** Manually trigger a check. Pass silent to suppress the "up to date" state. */
  checkForUpdates: (opts?: { silent?: boolean }) => Promise<void>;
  /** Download, install, and relaunch into the available update. */
  installUpdate: () => Promise<void>;
}

/**
 * Drives the Tauri updater flow: checks the GitHub release feed for a newer
 * signed build, exposes the result to the UI, and installs + relaunches on
 * demand. Updater config (endpoint + pubkey) lives in tauri.conf.json.
 *
 * The app checks once on launch (silently — we only surface a banner when an
 * update is actually available) and again whenever the user clicks the manual
 * "Check for updates" button (not silent, so "up to date" is shown as feedback).
 */
export function useUpdater(): UpdaterState {
  const [status, setStatus] = useState<UpdateStatus>("idle");
  const [newVersion, setNewVersion] = useState<string | null>(null);
  const [currentVersion, setCurrentVersion] = useState<string>("");
  const [notes, setNotes] = useState<string | null>(null);
  const [progress, setProgress] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Hold the resolved Update handle so installUpdate() can act on the exact
  // object returned by check() (it carries the download URL + signature).
  const updateRef = useRef<Update | null>(null);

  useEffect(() => {
    getVersion().then(setCurrentVersion).catch(() => {});
  }, []);

  const checkForUpdates = useCallback(async (opts?: { silent?: boolean }) => {
    const silent = opts?.silent ?? false;
    setError(null);
    setStatus("checking");
    try {
      const update = await check();
      if (update) {
        updateRef.current = update;
        setNewVersion(update.version);
        setNotes(update.body ?? null);
        setStatus("available");
      } else {
        updateRef.current = null;
        // On the silent launch check, stay quiet (idle) when up to date so we
        // don't flash a banner; manual checks report "up to date" as feedback.
        setStatus(silent ? "idle" : "uptodate");
      }
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      // The silent launch check must never nag: a missing release feed (before
      // the first signed release ships), an offline machine, or a transient
      // GitHub blip should not surface as an error banner. Only a check the
      // user explicitly triggered reports failures.
      if (silent) {
        console.warn("Silent update check failed:", message);
        setStatus("idle");
      } else {
        setError(message);
        setStatus("error");
      }
    }
  }, []);

  const installUpdate = useCallback(async () => {
    const update = updateRef.current;
    if (!update) return;
    setError(null);
    setStatus("downloading");
    setProgress(0);
    try {
      let downloaded = 0;
      let contentLength = 0;
      await update.downloadAndInstall((event) => {
        switch (event.event) {
          case "Started":
            contentLength = event.data.contentLength ?? 0;
            setProgress(0);
            break;
          case "Progress":
            downloaded += event.data.chunkLength;
            if (contentLength > 0) {
              setProgress(Math.round((downloaded / contentLength) * 100));
            }
            break;
          case "Finished":
            setProgress(100);
            setStatus("installing");
            break;
        }
      });
      // Download + install complete; relaunch into the new version. This call
      // terminates the current process, so nothing after it runs.
      await relaunch();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setStatus("error");
    }
  }, []);

  // Check once on launch, silently.
  useEffect(() => {
    checkForUpdates({ silent: true });
  }, [checkForUpdates]);

  return {
    status,
    newVersion,
    currentVersion,
    notes,
    progress,
    error,
    checkForUpdates,
    installUpdate,
  };
}
