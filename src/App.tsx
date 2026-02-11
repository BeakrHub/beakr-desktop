import Settings from "./components/Settings";
import PairingScreen from "./components/PairingScreen";
import { useAuth } from "./hooks/useAuth";

function App() {
  const { hasToken, clearToken } = useAuth();

  if (hasToken === null) {
    // Still loading token state from store
    return null;
  }

  if (!hasToken) {
    return (
      <div style={{ fontFamily: "system-ui, -apple-system, sans-serif" }}>
        <PairingScreen onPaired={() => window.location.reload()} />
      </div>
    );
  }

  return (
    <div style={{ fontFamily: "system-ui, -apple-system, sans-serif" }}>
      <Settings onUnlink={clearToken} />
    </div>
  );
}

export default App;
