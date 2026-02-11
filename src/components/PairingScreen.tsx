import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import BeakrLogo from "./BeakrLogo";

interface PairingScreenProps {
  onPaired: () => void;
}

export default function PairingScreen({ onPaired }: PairingScreenProps) {
  const [code, setCode] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const handlePair = async () => {
    const trimmed = code.trim().toUpperCase();
    if (trimmed.length !== 6) {
      setError("Code must be 6 characters");
      return;
    }

    setLoading(true);
    setError(null);

    try {
      await invoke("claim_pairing_code", { code: trimmed });
      onPaired();
    } catch (e) {
      setError(typeof e === "string" ? e : "Pairing failed. Check the code and try again.");
    } finally {
      setLoading(false);
    }
  };

  // Determine which environment we're pointing at (read from Rust side)
  const [envLabel, setEnvLabel] = useState("...");
  useEffect(() => {
    invoke<string>("get_ws_url").then((url) => {
      if (url.includes("sandbox")) setEnvLabel("Sandbox");
      else if (url.includes("localhost")) setEnvLabel("Local Dev");
      else setEnvLabel("Production");
    });
  }, []);

  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        alignItems: "center",
        justifyContent: "center",
        minHeight: "100vh",
        padding: "2rem",
        backgroundColor: "#f8f9fa",
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: "0.6rem",
          marginBottom: "1.5rem",
        }}
      >
        <BeakrLogo size={32} />
        <h1
          style={{
            fontSize: "1.5rem",
            fontWeight: 600,
            margin: 0,
            color: "#1a1a2e",
          }}
        >
          Beakr Desktop
        </h1>
      </div>

      <div
        style={{
          width: "100%",
          maxWidth: 360,
          background: "white",
          borderRadius: 12,
          padding: "2rem",
          boxShadow: "0 1px 3px rgba(0,0,0,0.1)",
        }}
      >
        <h2
          style={{
            fontSize: "1.1rem",
            fontWeight: 600,
            marginBottom: "0.5rem",
            textAlign: "center",
          }}
        >
          Pair Your Device
        </h2>
        <p
          style={{
            fontSize: "0.85rem",
            color: "#666",
            textAlign: "center",
            marginBottom: "1.5rem",
            lineHeight: 1.4,
          }}
        >
          Go to <strong>Settings &rarr; Local Files</strong> in the Beakr web app
          and click "Generate Pairing Code", then enter it below.
        </p>

        <input
          type="text"
          value={code}
          onChange={(e) => {
            // Allow only alphanumeric, max 6 chars
            const val = e.target.value.replace(/[^a-zA-Z0-9]/g, "").slice(0, 6);
            setCode(val.toUpperCase());
            setError(null);
          }}
          onKeyDown={(e) => e.key === "Enter" && handlePair()}
          placeholder="XXXXXX"
          maxLength={6}
          style={{
            width: "100%",
            padding: "0.75rem",
            fontSize: "1.5rem",
            fontFamily: "monospace",
            textAlign: "center",
            letterSpacing: "0.3em",
            border: "1px solid #ddd",
            borderRadius: 8,
            outline: "none",
            boxSizing: "border-box",
          }}
          autoFocus
        />

        {error && (
          <p
            style={{
              color: "#dc2626",
              fontSize: "0.8rem",
              marginTop: "0.75rem",
              textAlign: "center",
            }}
          >
            {error}
          </p>
        )}

        <button
          onClick={handlePair}
          disabled={loading || code.trim().length !== 6}
          style={{
            width: "100%",
            padding: "0.75rem",
            marginTop: "1rem",
            fontSize: "0.95rem",
            fontWeight: 600,
            background: loading || code.trim().length !== 6 ? "#9ca3af" : "#1a1a2e",
            color: "white",
            border: "none",
            borderRadius: 8,
            cursor: loading || code.trim().length !== 6 ? "not-allowed" : "pointer",
          }}
        >
          {loading ? "Pairing..." : "Pair Device"}
        </button>

        <p
          style={{
            fontSize: "0.75rem",
            color: "#9ca3af",
            textAlign: "center",
            marginTop: "1rem",
          }}
        >
          Environment: {envLabel}
        </p>
      </div>
    </div>
  );
}
