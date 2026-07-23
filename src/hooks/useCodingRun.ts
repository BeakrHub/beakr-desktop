import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { useEffect, useState } from "react";

export interface ActiveCodingRun {
  request_id: string;
  working_dir: string;
  cli: string;
  started_at_ms: number;
  status: "running" | "stopping";
}

/**
 * The active coding run, live (ENG-1552 run visibility). Initial state comes
 * from get_active_coding_run so a window opened mid-run catches up; after
 * that, coding_run:changed events carry every transition — including the
 * truth-telling "stopping" state, which only clears once the child process is
 * confirmed dead.
 */
export function useCodingRun() {
  const [run, setRun] = useState<ActiveCodingRun | null>(null);

  useEffect(() => {
    let cancelled = false;

    invoke<ActiveCodingRun | null>("get_active_coding_run")
      .then((current) => {
        if (!cancelled) setRun(current);
      })
      .catch(() => {
        // Command unavailable — leave as no-run; events still update us.
      });

    const unlisten = listen<{ active: boolean; run: ActiveCodingRun | null }>(
      "coding_run:changed",
      (event) => {
        setRun(event.payload.run ?? null);
      }
    );

    return () => {
      cancelled = true;
      unlisten.then((fn) => fn());
    };
  }, []);

  const stop = () => invoke<boolean>("stop_coding_run");

  return { run, stop };
}
