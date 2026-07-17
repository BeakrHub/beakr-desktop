import { useEffect, useState } from "react";
import { useCodingRun } from "../hooks/useCodingRun";

function basename(path: string): string {
  const parts = path.replace(/\/+$/, "").split("/");
  return parts[parts.length - 1] || path;
}

function formatElapsed(ms: number): string {
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

/**
 * The active coding run, rendered in the app window (ENG-1552 run
 * visibility). Nothing renders while no run is active. During a run: which
 * CLI, which folder (basename, full path on hover — the web card's
 * convention), a live elapsed timer, and Stop. After Stop is pressed the card
 * says "Stopping…" and stays until the child process is confirmed dead — the
 * card never claims a run is gone while it may still be editing files.
 */
export default function ActiveRunCard() {
  const { run, stop } = useCodingRun();
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (!run) return;
    const interval = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(interval);
  }, [run]);

  if (!run) return null;

  const stopping = run.status === "stopping";

  return (
    <div
      style={{
        padding: "1rem",
        border: "1px solid #e0e0e0",
        borderRadius: 8,
        backgroundColor: "#fafafa",
        marginBottom: "1rem",
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <span
          style={{
            width: 8,
            height: 8,
            borderRadius: "50%",
            backgroundColor: stopping ? "#f59e0b" : "#3b82f6",
            flexShrink: 0,
          }}
        />
        <div style={{ flex: 1, minWidth: 0 }}>
          <div style={{ fontWeight: 600, fontSize: 14 }}>
            {stopping ? "Stopping coding run…" : "Coding agent running"}
          </div>
          <div
            style={{
              fontSize: 12,
              color: "#6b7280",
              whiteSpace: "nowrap",
              overflow: "hidden",
              textOverflow: "ellipsis",
            }}
            title={run.working_dir}
          >
            {run.cli} · {basename(run.working_dir)} ·{" "}
            {formatElapsed(now - run.started_at_ms)}
          </div>
        </div>
        <button
          onClick={() => void stop()}
          disabled={stopping}
          style={{
            padding: "0.4rem 0.9rem",
            borderRadius: 6,
            border: "1px solid #ef4444",
            backgroundColor: stopping ? "#f3f4f6" : "#fff",
            color: stopping ? "#9ca3af" : "#ef4444",
            fontSize: 13,
            fontWeight: 600,
            cursor: stopping ? "default" : "pointer",
          }}
        >
          {stopping ? "Stopping…" : "Stop"}
        </button>
      </div>
    </div>
  );
}
