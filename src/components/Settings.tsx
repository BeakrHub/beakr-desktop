import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import BeakrLogo from "./BeakrLogo";
import ConnectionStatus from "./ConnectionStatus";
import ActiveRunCard from "./ActiveRunCard";
import ActivityFeed from "./ActivityFeed";
import FolderPicker from "./FolderPicker";
import CodingAgentSettings from "./CodingAgentSettings";
import SessionConnect from "./SessionConnect";
import UpdateBanner from "./UpdateBanner";
import { useUpdater } from "../hooks/useUpdater";

interface SettingsProps {
  onUnlink: () => void;
}

export default function Settings({ onUnlink }: SettingsProps) {
  const [deviceName, setDeviceName] = useState("");
  const [editingName, setEditingName] = useState(false);
  const [deviceNameError, setDeviceNameError] = useState<string | null>(null);
  const updater = useUpdater();

  useEffect(() => {
    invoke<string>("get_device_name")
      .then(setDeviceName)
      .catch(() => setDeviceNameError("Could not load device name."));
  }, []);

  const saveDeviceName = async () => {
    const trimmed = deviceName.trim();
    if (!trimmed) {
      setDeviceNameError("Device name cannot be empty.");
      return;
    }

    setDeviceNameError(null);
    try {
      await invoke("set_device_name", { name: trimmed });
      setDeviceName(trimmed);
      setEditingName(false);
    } catch (e) {
      setDeviceNameError(typeof e === "string" ? e : "Could not save device name.");
    }
  };

  return (
    <div style={{ padding: "1.5rem", maxWidth: 480, margin: "0 auto" }}>
      <div
        style={{
          display: "flex",
          justifyContent: "space-between",
          alignItems: "center",
          marginBottom: "1.5rem",
        }}
      >
        <div style={{ display: "flex", alignItems: "center", gap: "0.5rem" }}>
          <BeakrLogo size={24} />
          <h1 style={{ fontSize: "1.25rem", fontWeight: 600, margin: 0 }}>
            Beakr Desktop
          </h1>
        </div>
        <button
          onClick={onUnlink}
          style={{
            fontSize: "0.8rem",
            padding: "0.25rem 0.75rem",
            border: "1px solid #ddd",
            borderRadius: 6,
            background: "white",
            cursor: "pointer",
          }}
        >
          Unlink Device
        </button>
      </div>

      <UpdateBanner updater={updater} />

      <ConnectionStatus />

      <ActiveRunCard />

      <ActivityFeed />

      <section style={{ marginTop: "1.5rem" }}>
        <h2 style={{ fontSize: "1rem", fontWeight: 600, marginBottom: "0.75rem" }}>
          Device Name
        </h2>
        {editingName ? (
          <div style={{ display: "flex", gap: "0.5rem" }}>
            <input
              value={deviceName}
              onChange={(e) => setDeviceName(e.target.value)}
              onKeyDown={(e) => e.key === "Enter" && saveDeviceName()}
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
              onClick={saveDeviceName}
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
            <span style={{ fontSize: "0.9rem" }}>{deviceName}</span>
            <button
              onClick={() => setEditingName(true)}
              style={{
                fontSize: "0.8rem",
                padding: "0.25rem 0.5rem",
                border: "1px solid #ddd",
                borderRadius: 6,
                background: "white",
                cursor: "pointer",
              }}
            >
              Edit
            </button>
          </div>
        )}
        {deviceNameError && (
          <p
            role="alert"
            style={{
              color: "#dc2626",
              fontSize: "0.78rem",
              marginTop: "0.5rem",
              marginBottom: 0,
            }}
          >
            {deviceNameError}
          </p>
        )}
      </section>

      <FolderPicker />

      <CodingAgentSettings />

      <SessionConnect provider="benchling" displayName="Benchling" />

      <footer
        style={{
          marginTop: "1.5rem",
          paddingTop: "1rem",
          borderTop: "1px solid #eee",
          display: "flex",
          alignItems: "center",
          justifyContent: "space-between",
        }}
      >
        <span style={{ fontSize: "0.75rem", color: "#999" }}>
          Version {updater.currentVersion || "…"}
          {updater.status === "uptodate" && " — up to date"}
          {updater.status === "checking" && " — checking…"}
        </span>
        <button
          onClick={() => updater.checkForUpdates()}
          disabled={
            updater.status === "checking" ||
            updater.status === "downloading" ||
            updater.status === "installing"
          }
          style={{
            fontSize: "0.75rem",
            padding: "0.25rem 0.6rem",
            border: "1px solid #ddd",
            borderRadius: 6,
            background: "white",
            cursor: updater.status === "checking" ? "default" : "pointer",
            opacity: updater.status === "checking" ? 0.6 : 1,
          }}
        >
          Check for updates
        </button>
      </footer>
    </div>
  );
}
