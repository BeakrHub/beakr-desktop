import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

interface CodingAgentInfo {
  has_api_key: boolean;
  claude_binary_path: string | null;
}

/**
 * Settings for local coding-agent runs (ENG-1528): the user's own Anthropic
 * API key (v1 auth decision — used only by the local `claude` process) and an
 * optional binary-path override. The key is WRITE-ONLY: this UI only ever
 * learns whether one is set, never its value.
 */
export default function CodingAgentSettings() {
  const [hasKey, setHasKey] = useState(false);
  const [editing, setEditing] = useState(false);
  const [keyInput, setKeyInput] = useState("");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<CodingAgentInfo>("get_coding_agent_settings")
      .then((info) => setHasKey(info.has_api_key))
      .catch(() => setError("Could not load coding-agent settings."));
  }, []);

  const saveKey = async () => {
    const trimmed = keyInput.trim();
    if (!trimmed) {
      setError("API key cannot be empty.");
      return;
    }
    setError(null);
    try {
      await invoke("set_coding_agent_settings", { apiKey: trimmed });
      setHasKey(true);
      setKeyInput("");
      setEditing(false);
    } catch (e) {
      setError(typeof e === "string" ? e : "Could not save the API key.");
    }
  };

  return (
    <section style={{ marginTop: "1.5rem" }}>
      <h2 style={{ fontSize: "1rem", fontWeight: 600, marginBottom: "0.25rem" }}>
        Coding Agent
      </h2>
      <p style={{ fontSize: "0.78rem", color: "#666", marginTop: 0, marginBottom: "0.75rem" }}>
        Beakr can run Claude Code on this Mac when you ask it to. If you're
        logged into Claude Code (any Claude subscription), it works as-is —
        no setup needed. An API key is optional, only if you'd rather use one.
      </p>
      {editing ? (
        <div style={{ display: "flex", gap: "0.5rem" }}>
          <input
            type="password"
            placeholder="sk-ant-…"
            value={keyInput}
            onChange={(e) => setKeyInput(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && saveKey()}
            style={{
              flex: 1,
              padding: "0.5rem",
              border: "1px solid #ddd",
              borderRadius: 6,
              fontSize: "0.9rem",
            }}
            autoFocus
          />
          <button
            onClick={saveKey}
            style={{
              padding: "0.5rem 1rem",
              background: "#1a1a2e",
              color: "white",
              border: "none",
              borderRadius: 6,
              cursor: "pointer",
            }}
          >
            Save
          </button>
        </div>
      ) : (
        <div
          style={{
            display: "flex",
            justifyContent: "space-between",
            alignItems: "center",
          }}
        >
          <span style={{ fontSize: "0.9rem" }}>
            {hasKey ? "Anthropic API key set ✓" : "Using Claude Code login (no API key)"}
          </span>
          <button
            onClick={() => setEditing(true)}
            style={{
              fontSize: "0.8rem",
              padding: "0.25rem 0.5rem",
              border: "1px solid #ddd",
              borderRadius: 6,
              background: "white",
              cursor: "pointer",
            }}
          >
            {hasKey ? "Replace" : "Add key"}
          </button>
        </div>
      )}
      {error && (
        <p
          role="alert"
          style={{
            color: "#dc2626",
            fontSize: "0.78rem",
            marginTop: "0.5rem",
            marginBottom: 0,
          }}
        >
          {error}
        </p>
      )}
    </section>
  );
}
