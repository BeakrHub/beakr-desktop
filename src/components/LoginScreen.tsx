import { SignIn } from "@clerk/clerk-react";
import BeakrLogo from "./BeakrLogo";

export default function LoginScreen() {
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
      <SignIn
        appearance={{
          elements: {
            rootBox: { width: "100%" },
          },
        }}
      />
    </div>
  );
}
