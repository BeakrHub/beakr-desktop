import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/**
 * Manages device token lifecycle:
 * 1. On mount: read stored token, pass to Rust, auto-connect
 * 2. Listen for `token_invalid` event from Rust (revoked) → clear store
 *
 * No periodic refresh needed — device tokens are long-lived.
 */
export function useAuth() {
  const [hasToken, setHasToken] = useState<boolean | null>(null);
  const didConnect = useRef(false);

  useEffect(() => {
    let cancelled = false;

    async function init() {
      try {
        // Check if we have a stored token
        const token = await invoke<string | null>("get_stored_token");
        if (cancelled) return;

        if (token) {
          // Pass token to Rust WS client and connect
          await invoke("set_auth_token", { token });
          setHasToken(true);

          if (!didConnect.current) {
            didConnect.current = true;
            invoke("connect_ws");
          }
        } else {
          setHasToken(false);
        }
      } catch {
        if (!cancelled) setHasToken(false);
      }
    }

    init();

    // Listen for token_invalid event (device revoked on server)
    const unlisten = listen("token_invalid", () => {
      invoke("clear_token").catch(() => {});
      setHasToken(false);
      didConnect.current = false;
    });

    return () => {
      cancelled = true;
      unlisten.then((fn) => fn());
    };
  }, []);

  const clearToken = async () => {
    try {
      await invoke("disconnect_ws");
      await invoke("clear_token");
    } catch {
      // ignore
    }
    setHasToken(false);
    didConnect.current = false;
  };

  return { hasToken, clearToken };
}
