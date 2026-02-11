import { useEffect, useRef } from "react";
import { useAuth as useClerkAuth } from "@clerk/clerk-react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const TOKEN_REFRESH_INTERVAL = 50_000; // 50 seconds
const IS_DEV = !import.meta.env.VITE_CLERK_PUBLISHABLE_KEY;

/**
 * Manages the Clerk JWT lifecycle:
 * 1. On sign-in: pass token to Rust and auto-connect
 * 2. Every 50s: proactively refresh and pass to Rust
 * 3. On `token_refresh_needed` event from Rust: immediately refresh
 *
 * In dev mode (no Clerk key), skips token management and auto-connects.
 */
export function useAuth() {
  const clerkAuth = IS_DEV ? null : useClerkAuth();
  const intervalRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const didConnect = useRef(false);

  // Dev mode: just auto-connect once, no token needed
  useEffect(() => {
    if (!IS_DEV || didConnect.current) return;
    didConnect.current = true;
    invoke("connect_ws");
  }, []);

  // Production mode: full Clerk token lifecycle
  useEffect(() => {
    if (IS_DEV || !clerkAuth?.isSignedIn) return;

    const refreshToken = async () => {
      try {
        const token = await clerkAuth.getToken({ template: "beakr" });
        if (token) {
          await invoke("set_auth_token", { token });
        }
      } catch (e) {
        console.error("Failed to refresh token:", e);
      }
    };

    // Initial token pass + connect
    refreshToken().then(() => {
      invoke("connect_ws");
    });

    // Periodic refresh
    intervalRef.current = setInterval(refreshToken, TOKEN_REFRESH_INTERVAL);

    // Listen for Rust requesting a fresh token (during reconnection)
    const unlisten = listen("token_refresh_needed", () => {
      refreshToken();
    });

    return () => {
      if (intervalRef.current) clearInterval(intervalRef.current);
      unlisten.then((fn) => fn());
    };
  }, [IS_DEV ? false : clerkAuth?.isSignedIn]);
}
