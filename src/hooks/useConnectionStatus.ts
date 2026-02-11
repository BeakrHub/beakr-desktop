import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";

interface ConnectionState {
  status: string;
  deviceId: string | null;
}

/**
 * Listen for WebSocket status changes from the Rust backend.
 */
export function useConnectionStatus(): ConnectionState {
  const [status, setStatus] = useState("disconnected");
  const [deviceId, setDeviceId] = useState<string | null>(null);

  useEffect(() => {
    // Get initial status
    invoke<{ status: string; device_id: string | null }>("get_connection_status")
      .then((result) => {
        // Status comes as a JSON value — normalize to lowercase string
        const s = typeof result.status === "string"
          ? result.status.toLowerCase()
          : "disconnected";
        setStatus(s);
        setDeviceId(result.device_id);
      });

    // Listen for status changes
    const unlisten = listen<string | null>("ws:status_changed", (event) => {
      const payload = event.payload;
      if (typeof payload === "string") {
        setStatus(payload.toLowerCase().replace(/"/g, ""));
      }

      // Refresh device_id from Rust
      invoke<{ status: string; device_id: string | null }>("get_connection_status")
        .then((result) => {
          setDeviceId(result.device_id);
        });
    });

    return () => {
      unlisten.then((fn) => fn());
    };
  }, []);

  return { status, deviceId };
}
