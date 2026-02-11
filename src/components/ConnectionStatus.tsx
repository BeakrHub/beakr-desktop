import { useConnectionStatus } from "../hooks/useConnectionStatus";
import { invoke } from "@tauri-apps/api/core";

export default function ConnectionStatus() {
  const { status, deviceId } = useConnectionStatus();

  const isConnected = status === "connected";
  const isRevoked = status === "revoked";

  const dotColor = isConnected ? "#22c55e" : isRevoked ? "#ef4444" : "#9ca3af";

  const statusText: Record<string, string> = {
    disconnected: "Disconnected",
    connecting: "Connecting…",
    connected: "Connected",
    reconnecting: "Reconnecting…",
    revoked: "Device Revoked",
  };

  const handleConnect = () => invoke("connect_ws");
  const handleDisconnect = () => invoke("disconnect_ws");

  return (
    <div
      style={{
        padding: "1rem",
        border: "1px solid #e0e0e0",
        borderRadius: 8,
        backgroundColor: "#fafafa",
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "0.5rem" }}>
          <span
            style={{
              width: 10,
              height: 10,
              borderRadius: "50%",
              backgroundColor: dotColor,
              display: "inline-block",
            }}
          />
          <span style={{ fontSize: "0.9rem", fontWeight: 500 }}>
            {statusText[status] || status}
          </span>
        </div>

        {isConnected ? (
          <button
            onClick={handleDisconnect}
            style={{
              fontSize: "0.8rem",
              padding: "0.25rem 0.75rem",
              border: "1px solid #ddd",
              borderRadius: 6,
              background: "white",
              cursor: "pointer",
            }}
          >
            Disconnect
          </button>
        ) : !isRevoked ? (
          <button
            onClick={handleConnect}
            style={{
              fontSize: "0.8rem",
              padding: "0.25rem 0.75rem",
              border: "none",
              borderRadius: 6,
              background: "#1a1a2e",
              color: "white",
              cursor: "pointer",
            }}
          >
            Connect
          </button>
        ) : null}
      </div>

      {deviceId && (
        <p
          style={{
            fontSize: "0.75rem",
            color: "#999",
            marginTop: "0.5rem",
            marginBottom: 0,
            fontFamily: "monospace",
          }}
        >
          Device ID: {deviceId}
        </p>
      )}
    </div>
  );
}
