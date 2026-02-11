import { SignedIn, SignedOut } from "@clerk/clerk-react";
import LoginScreen from "./components/LoginScreen";
import Settings from "./components/Settings";
import { useAuth } from "./hooks/useAuth";

const IS_DEV = !import.meta.env.VITE_CLERK_PUBLISHABLE_KEY;

function App() {
  // Start token lifecycle (refresh loop, pass to Rust)
  // In dev mode (no Clerk key), useAuth is a no-op and we connect via dev query params
  useAuth();

  if (IS_DEV) {
    // Dev mode: skip Clerk auth entirely, show settings directly
    return (
      <div style={{ fontFamily: "system-ui, -apple-system, sans-serif" }}>
        <Settings />
      </div>
    );
  }

  return (
    <div style={{ fontFamily: "system-ui, -apple-system, sans-serif" }}>
      <SignedOut>
        <LoginScreen />
      </SignedOut>
      <SignedIn>
        <Settings />
      </SignedIn>
    </div>
  );
}

export default App;
