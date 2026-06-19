import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

interface ProgressPayload {
  provider: string;
  stage: string;
  message: string;
  current: number | null;
  total: number | null;
}

interface DonePayload {
  provider: string;
  received?: number | null;
  sync_job_id?: string | null;
  items_sent?: number | null;
}

interface MessagePayload {
  provider: string;
  message: string;
}

type StatusKind = "idle" | "running" | "needs_login" | "done" | "error";

interface SessionConnectProps {
  /** Provider key registered in the Rust registry (e.g. "benchling", "labarchives"). */
  provider: string;
  /** Human-readable name shown in the UI. */
  displayName: string;
  /** Optional override for the description blurb. */
  description?: string;
}

/**
 * Generic session connector UI. Opens the provider's site in a webview where the
 * user logs in with their own session, then imports their data into Beakr.
 *
 * Drives the Rust `connect_session` / `session_import` commands with this
 * component's `provider`, and listens to the generic `session:*` events, filtering
 * to only those whose payload `provider` matches — so multiple connectors can be
 * rendered side by side without cross-talk.
 */
export default function SessionConnect({
  provider,
  displayName,
  description,
}: SessionConnectProps) {
  const [status, setStatus] = useState<StatusKind>("idle");
  const [message, setMessage] = useState("");
  const [opened, setOpened] = useState(false);

  useEffect(() => {
    const unlisteners: Array<Promise<() => void>> = [];

    unlisteners.push(
      listen<ProgressPayload>("session:progress", (event) => {
        if (event.payload?.provider !== provider) return;
        const p = event.payload;
        setStatus("running");
        const counter =
          p.current != null && p.total != null ? ` (${p.current}/${p.total})` : "";
        setMessage(`${p.message}${counter}`);
      })
    );

    unlisteners.push(
      listen<MessagePayload>("session:needs_login", (event) => {
        if (event.payload?.provider !== provider) return;
        setStatus("needs_login");
        setMessage(
          event.payload?.message ||
            `Please log in to ${displayName} in the window, then click Import.`
        );
      })
    );

    unlisteners.push(
      listen<DonePayload>("session:done", (event) => {
        if (event.payload?.provider !== provider) return;
        setStatus("done");
        const sent = event.payload?.items_sent ?? event.payload?.received ?? 0;
        setMessage(`Imported ${sent} item${sent === 1 ? "" : "s"} into Beakr.`);
      })
    );

    unlisteners.push(
      listen<MessagePayload>("session:error", (event) => {
        if (event.payload?.provider !== provider) return;
        setStatus("error");
        setMessage(event.payload?.message || `${displayName} import failed.`);
      })
    );

    return () => {
      unlisteners.forEach((u) => u.then((fn) => fn()));
    };
  }, [provider, displayName]);

  const handleConnect = async () => {
    setStatus("idle");
    setMessage(`Opening ${displayName}… log in there, then click Import.`);
    try {
      await invoke("connect_session", { provider });
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
      await invoke("session_import", { provider });
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

  const blurb =
    description ??
    `Connect your ${displayName} account to import your data into Beakr. You log in with your own ${displayName} session — Beakr never sees your password.`;

  return (
    <section style={{ marginTop: "1.5rem" }}>
      <h2 style={{ fontSize: "1rem", fontWeight: 600, marginBottom: "0.75rem" }}>
        {displayName}
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
          {blurb}
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
            Connect {displayName}
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
