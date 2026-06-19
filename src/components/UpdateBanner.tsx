import type { UpdaterState } from "../hooks/useUpdater";

/**
 * Top-of-window banner that appears only when an update is available, being
 * downloaded/installed, or failed. The idle / checking / up-to-date states are
 * surfaced quietly in the Settings footer instead (see Settings.tsx), so this
 * banner stays out of the way until there is something for the user to act on.
 */
export default function UpdateBanner({ updater }: { updater: UpdaterState }) {
  const { status, newVersion, notes, progress, error, installUpdate } = updater;

  const isActionable =
    status === "available" ||
    status === "downloading" ||
    status === "installing" ||
    status === "error";

  if (!isActionable) return null;

  const busy = status === "downloading" || status === "installing";

  const headline =
    status === "error"
      ? "Update failed"
      : status === "downloading"
      ? `Downloading update${progress != null ? ` — ${progress}%` : "…"}`
      : status === "installing"
      ? "Installing — the app will restart…"
      : `Update available — version ${newVersion}`;

  return (
    <div
      style={{
        padding: "0.75rem 1rem",
        borderRadius: 8,
        marginBottom: "1.25rem",
        backgroundColor: status === "error" ? "#fef2f2" : "#eef2ff",
        border: `1px solid ${status === "error" ? "#fecaca" : "#c7d2fe"}`,
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
          gap: "0.75rem",
        }}
      >
        <span style={{ fontSize: "0.9rem", fontWeight: 600 }}>{headline}</span>

        {status === "available" && (
          <button
            onClick={installUpdate}
            style={{
              fontSize: "0.8rem",
              padding: "0.35rem 0.85rem",
              border: "none",
              borderRadius: 6,
              background: "#1a1a2e",
              color: "white",
              cursor: "pointer",
              whiteSpace: "nowrap",
            }}
          >
            Update &amp; restart
          </button>
        )}

        {status === "error" && (
          <button
            onClick={installUpdate}
            style={{
              fontSize: "0.8rem",
              padding: "0.35rem 0.85rem",
              border: "1px solid #fecaca",
              borderRadius: 6,
              background: "white",
              cursor: "pointer",
              whiteSpace: "nowrap",
            }}
          >
            Retry
          </button>
        )}
      </div>

      {/* Progress bar while downloading */}
      {busy && (
        <div
          style={{
            marginTop: "0.6rem",
            height: 6,
            borderRadius: 999,
            backgroundColor: "#dbeafe",
            overflow: "hidden",
          }}
        >
          <div
            style={{
              height: "100%",
              width: `${status === "installing" ? 100 : progress ?? 0}%`,
              backgroundColor: "#4f46e5",
              transition: "width 0.2s ease",
            }}
          />
        </div>
      )}

      {/* Release notes for an available update */}
      {status === "available" && notes && (
        <p
          style={{
            fontSize: "0.78rem",
            color: "#4b5563",
            marginTop: "0.6rem",
            marginBottom: 0,
            whiteSpace: "pre-wrap",
          }}
        >
          {notes}
        </p>
      )}

      {status === "error" && error && (
        <p
          style={{
            fontSize: "0.78rem",
            color: "#b91c1c",
            marginTop: "0.5rem",
            marginBottom: 0,
            wordBreak: "break-word",
          }}
        >
          {error}
        </p>
      )}
    </div>
  );
}
