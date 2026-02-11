import { useUser, useClerk } from "@clerk/clerk-react";
import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import BeakrLogo from "./BeakrLogo";
import ConnectionStatus from "./ConnectionStatus";
import ActivityFeed from "./ActivityFeed";
import FolderPicker from "./FolderPicker";

const IS_DEV = !import.meta.env.VITE_CLERK_PUBLISHABLE_KEY;

export default function Settings() {
  const clerkUser = IS_DEV ? null : useUser();
  const clerk = IS_DEV ? null : useClerk();
  const [deviceName, setDeviceName] = useState("");
  const [editingName, setEditingName] = useState(false);

  useEffect(() => {
    invoke<string>("get_device_name").then(setDeviceName);
  }, []);

  const saveDeviceName = async () => {
    if (deviceName.trim()) {
      await invoke("set_device_name", { name: deviceName.trim() });
    }
    setEditingName(false);
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
        {IS_DEV ? (
          <span
            style={{
              fontSize: "0.75rem",
              padding: "0.2rem 0.5rem",
              background: "#fef3c7",
              color: "#92400e",
              borderRadius: 4,
            }}
          >
            Dev Mode
          </span>
        ) : (
          <button
            onClick={() => clerk?.signOut()}
            style={{
              fontSize: "0.8rem",
              padding: "0.25rem 0.75rem",
              border: "1px solid #ddd",
              borderRadius: 6,
              background: "white",
              cursor: "pointer",
            }}
          >
            Sign Out
          </button>
        )}
      </div>

      {IS_DEV ? (
        <p style={{ color: "#666", fontSize: "0.85rem", marginBottom: "1.5rem" }}>
          Dev mode — using local dev credentials
        </p>
      ) : clerkUser?.user ? (
        <p style={{ color: "#666", fontSize: "0.85rem", marginBottom: "1.5rem" }}>
          Signed in as {clerkUser.user.primaryEmailAddress?.emailAddress}
        </p>
      ) : null}

      <ConnectionStatus />

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
      </section>

      <FolderPicker />
    </div>
  );
}
