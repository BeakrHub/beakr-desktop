import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

interface CliReadiness {
  cli: "claude" | "codex";
  installed: boolean;
  binary_path?: string;
  version?: string;
  login: "signed_in" | "not_signed_in" | "unknown";
  ready: boolean;
}

interface CodingAgentInfo {
  has_api_key: boolean;
  claude_binary_path: string | null;
  default_cli: string | null;
}

const CLI_META: Record<
  CliReadiness["cli"],
  { label: string; installCmd: string; installUrl: string; loginHint: string }
> = {
  claude: {
    label: "Claude Code",
    installCmd: "npm install -g @anthropic-ai/claude-code",
    installUrl: "https://claude.ai/code",
    loginHint: "Open a terminal, run `claude`, then `/login`.",
  },
  codex: {
    label: "Codex",
    installCmd: "npm install -g @openai/codex",
    installUrl: "https://developers.openai.com/codex/cli",
    loginHint: "Open a terminal and run `codex login`.",
  },
};

function statusFor(r: CliReadiness): { dot: string; text: string; guidance?: string } {
  const meta = CLI_META[r.cli];
  if (!r.installed) {
    return {
      dot: "#9ca3af",
      text: "Not installed",
      guidance: `Install the ${meta.label} CLI to use it here: ${meta.installCmd}`,
    };
  }
  if (r.login === "not_signed_in") {
    return {
      dot: "#ef4444",
      text: "Installed, not signed in",
      guidance: meta.loginHint,
    };
  }
  if (r.login === "unknown") {
    return {
      dot: "#f59e0b",
      text: "Installed — sign-in verifies on first run",
    };
  }
  return { dot: "#22c55e", text: "Ready" };
}

/**
 * Per-CLI readiness + default-CLI picker (ENG-1536). Readiness comes from
 * free signals only (credential presence + last real run's outcome) — the
 * backend never probes a CLI, because a probe against a logged-in CLI is a
 * real API call on the user's own plan. The API key stays WRITE-ONLY: this
 * UI only ever learns whether one is set, never its value.
 */
export default function CodingAgentSettings() {
  const [readiness, setReadiness] = useState<CliReadiness[] | null>(null);
  const [defaultCli, setDefaultCli] = useState<string>("claude");
  const [hasKey, setHasKey] = useState(false);
  const [editingKey, setEditingKey] = useState(false);
  const [keyInput, setKeyInput] = useState("");
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    invoke<CliReadiness[]>("get_coding_agent_readiness")
      .then(setReadiness)
      .catch(() => setError("Could not detect coding agents."));
    invoke<CodingAgentInfo>("get_coding_agent_settings")
      .then((info) => {
        setHasKey(info.has_api_key);
        setDefaultCli(info.default_cli ?? "claude");
      })
      .catch(() => setError("Could not load coding-agent settings."));
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

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
      setEditingKey(false);
    } catch (e) {
      setError(typeof e === "string" ? e : "Could not save the API key.");
    }
  };

  const pickDefault = async (cli: string) => {
    const prev = defaultCli;
    setDefaultCli(cli);
    try {
      await invoke("set_coding_agent_settings", { defaultCli: cli });
    } catch (e) {
      setDefaultCli(prev);
      setError(typeof e === "string" ? e : "Could not save the default CLI.");
    }
  };

  const installedClis = (readiness ?? []).filter((r) => r.installed);
  const noneDetected = readiness !== null && installedClis.length === 0;

  return (
    <section style={{ marginTop: "1.5rem" }}>
      <h2 style={{ fontSize: "1rem", fontWeight: 600, marginBottom: "0.25rem" }}>
        Coding Agent
      </h2>
      <p style={{ fontSize: "0.78rem", color: "#666", marginTop: 0, marginBottom: "0.75rem" }}>
        Beakr can run a coding agent CLI on this Mac when you ask it to. Each
        CLI uses its own login and your own plan — Beakr never handles the
        credential.
      </p>

      {noneDetected && (
        <div
          style={{
            padding: "0.75rem",
            border: "1px solid #fcd34d",
            background: "#fffbeb",
            borderRadius: 8,
            fontSize: "0.82rem",
            marginBottom: "0.75rem",
          }}
        >
          No coding agents detected on this Mac. Install one to use this
          feature — Claude Code: <code>{CLI_META.claude.installCmd}</code> or
          Codex: <code>{CLI_META.codex.installCmd}</code> — then sign in and
          reopen this window.
        </div>
      )}

      {(readiness ?? []).map((r) => {
        const meta = CLI_META[r.cli];
        const status = statusFor(r);
        return (
          <div
            key={r.cli}
            style={{
              padding: "0.75rem",
              border: "1px solid #e5e7eb",
              borderRadius: 8,
              marginBottom: "0.5rem",
            }}
          >
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <span
                style={{
                  width: 8,
                  height: 8,
                  borderRadius: "50%",
                  backgroundColor: status.dot,
                  flexShrink: 0,
                }}
              />
              <span style={{ fontWeight: 600, fontSize: "0.9rem" }}>{meta.label}</span>
              {r.version && (
                <span style={{ fontSize: "0.72rem", color: "#9ca3af" }}>{r.version}</span>
              )}
              <span style={{ marginLeft: "auto", fontSize: "0.8rem", color: "#4b5563" }}>
                {status.text}
                {r.cli === "claude" && r.ready && hasKey ? " · API key set" : ""}
              </span>
            </div>
            {status.guidance && (
              <p style={{ fontSize: "0.76rem", color: "#666", margin: "0.4rem 0 0 16px" }}>
                {status.guidance}{" "}
                {!r.installed && (
                  <a href={meta.installUrl} target="_blank" rel="noreferrer">
                    Install guide ↗
                  </a>
                )}
              </p>
            )}
            {r.cli === "claude" && r.installed && (
              <div style={{ margin: "0.5rem 0 0 16px" }}>
                {editingKey ? (
                  <div style={{ display: "flex", gap: "0.5rem" }}>
                    <input
                      type="password"
                      placeholder="sk-ant-…"
                      value={keyInput}
                      onChange={(e) => setKeyInput(e.target.value)}
                      onKeyDown={(e) => e.key === "Enter" && saveKey()}
                      style={{
                        flex: 1,
                        padding: "0.4rem",
                        border: "1px solid #ddd",
                        borderRadius: 6,
                        fontSize: "0.85rem",
                      }}
                      autoFocus
                    />
                    <button
                      onClick={saveKey}
                      style={{
                        padding: "0.4rem 0.8rem",
                        background: "#1a1a2e",
                        color: "white",
                        border: "none",
                        borderRadius: 6,
                        cursor: "pointer",
                        fontSize: "0.8rem",
                      }}
                    >
                      Save
                    </button>
                  </div>
                ) : (
                  <button
                    onClick={() => setEditingKey(true)}
                    style={{
                      fontSize: "0.74rem",
                      padding: "0.2rem 0.5rem",
                      border: "1px solid #ddd",
                      borderRadius: 6,
                      background: "white",
                      cursor: "pointer",
                      color: "#4b5563",
                    }}
                  >
                    {hasKey ? "Replace API key" : "Add API key (optional)"}
                  </button>
                )}
              </div>
            )}
          </div>
        );
      })}

      {installedClis.length > 1 && (
        <div style={{ marginTop: "0.75rem" }}>
          <div style={{ fontSize: "0.8rem", fontWeight: 600, marginBottom: "0.35rem" }}>
            Default CLI
          </div>
          <p style={{ fontSize: "0.74rem", color: "#666", margin: "0 0 0.4rem 0" }}>
            Used when you don't name one in your request. Asking for a specific
            CLI in chat still overrides this.
          </p>
          <div style={{ display: "flex", gap: "1rem" }}>
            {installedClis.map((r) => (
              <label
                key={r.cli}
                style={{ fontSize: "0.85rem", display: "flex", alignItems: "center", gap: 6 }}
              >
                <input
                  type="radio"
                  name="default-cli"
                  checked={defaultCli === r.cli}
                  onChange={() => pickDefault(r.cli)}
                />
                {CLI_META[r.cli].label}
              </label>
            ))}
          </div>
        </div>
      )}

      {error && (
        <p
          role="alert"
          style={{ color: "#dc2626", fontSize: "0.78rem", marginTop: "0.5rem", marginBottom: 0 }}
        >
          {error}
        </p>
      )}
    </section>
  );
}
