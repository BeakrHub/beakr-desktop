import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

interface ProgressPayload {
  stage: string;
  message: string;
  current: number | null;
  total: number | null;
}

interface DonePayload {
  received?: number | null;
  sync_job_id?: string | null;
  items_sent?: number | null;
}

type StatusKind = "idle" | "running" | "needs_login" | "done" | "error";

/**
 * Connect Benchling: opens benchling.com in a webview where the user logs in
 * with their own session, then imports their folders/entries/sequences into
 * Beakr. Listens to the `benchling:*` events emitted by the Rust side.
 */
export default function BenchlingConnect() {
  const [status, setStatus] = useState<StatusKind>("idle");
  const [message, setMessage] = useState("");
  const [opened, setOpened] = useState(false);

  useEffect(() => {
    const unlisteners: Array<Promise<() => void>> = [];

    unlisteners.push(
      listen<ProgressPayload>("benchling:progress", (event) => {
        const p = event.payload;
        setStatus("running");
        const counter =
          p.current != null && p.total != null ? ` (${p.current}/${p.total})` : "";
        setMessage(`${p.message}${counter}`);
      })
    );

    unlisteners.push(
      listen<{ message: string }>("benchling:needs_login", (event) => {
        setStatus("needs_login");
        setMessage(
          event.payload?.message ||
            "Please log in to Benchling in the window, then click Import."
        );
      })
    );

    unlisteners.push(
      listen<DonePayload>("benchling:done", (event) => {
        setStatus("done");
        const sent = event.payload?.items_sent ?? event.payload?.received ?? 0;
        setMessage(`Imported ${sent} item${sent === 1 ? "" : "s"} into Beakr.`);
      })
    );

    unlisteners.push(
      listen<{ message: string }>("benchling:error", (event) => {
        setStatus("error");
        setMessage(event.payload?.message || "Benchling import failed.");
      })
    );

    return () => {
      unlisteners.forEach((u) => u.then((fn) => fn()));
    };
  }, []);

  const handleConnect = async () => {
    setStatus("idle");
    setMessage("Opening Benchling… log in there, then click Import.");
    try {
      await invoke("connect_benchling");
      setOpened(true);
    } catch (e) {
      setStatus("error");
      setMessage(String(e));
    }
  };

  const handleImport = async () => {
    setStatus("running");
    setMessage("Starting import…");
    try {
      await invoke("benchling_import");
    } catch (e) {
      setStatus("error");
      setMessage(String(e));
    }
  };

  const dotColor =
    status === "done"
      ? "#22c55e"
      : status === "error"
      ? "#ef4444"
      : status === "needs_login"
      ? "#f59e0b"
      : status === "running"
      ? "#3b82f6"
      : "#9ca3af";

  return (
    <section style={{ marginTop: "1.5rem" }}>
      <h2 style={{ fontSize: "1rem", fontWeight: 600, marginBottom: "0.75rem" }}>
        Benchling
      </h2>
      <div
        style={{
          padding: "1rem",
          border: "1px solid #e0e0e0",
          borderRadius: 8,
          backgroundColor: "#fafafa",
        }}
      >
        <p
          style={{
            fontSize: "0.8rem",
            color: "#555",
            marginTop: 0,
            marginBottom: "0.75rem",
          }}
        >
          Connect your Benchling account to import folders, entries, and sequences
          into Beakr. You log in with your own Benchling session — Beakr never sees
          your password.
        </p>

        <div style={{ display: "flex", gap: "0.5rem" }}>
          <button
            onClick={handleConnect}
            style={{
              fontSize: "0.8rem",
              padding: "0.4rem 0.9rem",
              border: "none",
              borderRadius: 6,
              background: "#1a1a2e",
              color: "white",
              cursor: "pointer",
            }}
          >
            Connect Benchling
          </button>
          <button
            onClick={handleImport}
            disabled={!opened}
            style={{
              fontSize: "0.8rem",
              padding: "0.4rem 0.9rem",
              border: "1px solid #ddd",
              borderRadius: 6,
              background: opened ? "white" : "#f0f0f0",
              color: opened ? "#1a1a2e" : "#aaa",
              cursor: opened ? "pointer" : "not-allowed",
            }}
          >
            Import
          </button>
        </div>

        {message && (
          <div
            style={{
              display: "flex",
              alignItems: "center",
              gap: "0.5rem",
              marginTop: "0.75rem",
            }}
          >
            <span
              style={{
                width: 8,
                height: 8,
                borderRadius: "50%",
                backgroundColor: dotColor,
                display: "inline-block",
                flexShrink: 0,
              }}
            />
            <span style={{ fontSize: "0.78rem", color: "#444" }}>{message}</span>
          </div>
        )}
      </div>
    </section>
  );
}
