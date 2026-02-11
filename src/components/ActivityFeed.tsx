import { useActivityFeed, type ActivityEvent } from "../hooks/useActivityFeed";

const TOOL_LABELS: Record<string, string> = {
  list_files: "Listing files",
  search_files: "Searching files",
  read_file: "Reading file",
  file_info: "Getting file info",
};

function toolIcon(tool: string): string {
  switch (tool) {
    case "list_files":
      return "\u{1F4C2}";
    case "search_files":
      return "\u{1F50D}";
    case "read_file":
      return "\u{1F4C4}";
    case "file_info":
      return "\u{2139}\uFE0F";
    default:
      return "\u{2699}\uFE0F";
  }
}

function statusDot(status: ActivityEvent["status"]): React.ReactNode {
  if (status === "pending") {
    return (
      <span
        style={{
          display: "inline-block",
          width: 8,
          height: 8,
          borderRadius: "50%",
          backgroundColor: "#f59e0b",
          animation: "pulse 1s ease-in-out infinite",
          flexShrink: 0,
        }}
      />
    );
  }
  const color = status === "success" ? "#22c55e" : "#ef4444";
  return (
    <span
      style={{
        display: "inline-block",
        width: 8,
        height: 8,
        borderRadius: "50%",
        backgroundColor: color,
        flexShrink: 0,
      }}
    />
  );
}

function describeParams(_tool: string, params: Record<string, unknown>): string {
  const path = params.path as string | undefined;
  const query = params.query as string | undefined;

  if (path) {
    // Show just the filename or last path segment
    const segments = path.split("/");
    return segments[segments.length - 1] || path;
  }
  if (query) {
    return `"${query}"`;
  }
  return "";
}

function timeAgo(timestamp: number): string {
  const seconds = Math.floor((Date.now() - timestamp) / 1000);
  if (seconds < 5) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ago`;
}

export default function ActivityFeed() {
  const events = useActivityFeed();

  if (events.length === 0) {
    return null;
  }

  return (
    <section style={{ marginTop: "1.5rem" }}>
      <style>{`
        @keyframes pulse {
          0%, 100% { opacity: 1; }
          50% { opacity: 0.4; }
        }
        @keyframes slideIn {
          from { opacity: 0; transform: translateY(-8px); }
          to { opacity: 1; transform: translateY(0); }
        }
      `}</style>
      <h2
        style={{
          fontSize: "1rem",
          fontWeight: 600,
          marginBottom: "0.75rem",
          display: "flex",
          alignItems: "center",
          gap: "0.5rem",
        }}
      >
        Activity
        {events.some((e) => e.status === "pending") && (
          <span
            style={{
              fontSize: "0.7rem",
              padding: "0.1rem 0.4rem",
              background: "#fef3c7",
              color: "#92400e",
              borderRadius: 4,
              fontWeight: 500,
            }}
          >
            Active
          </span>
        )}
      </h2>

      <div
        style={{
          border: "1px solid #e0e0e0",
          borderRadius: 8,
          overflow: "hidden",
          maxHeight: 240,
          overflowY: "auto",
        }}
      >
        {events.map((event, i) => (
          <div
            key={event.id}
            style={{
              display: "flex",
              alignItems: "center",
              gap: "0.5rem",
              padding: "0.5rem 0.75rem",
              borderBottom:
                i < events.length - 1 ? "1px solid #f0f0f0" : "none",
              backgroundColor:
                event.status === "pending" ? "#fffbeb" : "white",
              animation: "slideIn 0.2s ease-out",
              fontSize: "0.8rem",
            }}
          >
            <span style={{ flexShrink: 0, fontSize: "0.9rem" }}>
              {toolIcon(event.tool)}
            </span>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: "0.4rem",
                }}
              >
                {statusDot(event.status)}
                <span style={{ fontWeight: 500 }}>
                  {TOOL_LABELS[event.tool] || event.tool}
                </span>
              </div>
              {describeParams(event.tool, event.params) && (
                <div
                  style={{
                    color: "#666",
                    fontSize: "0.75rem",
                    marginTop: "0.1rem",
                    overflow: "hidden",
                    textOverflow: "ellipsis",
                    whiteSpace: "nowrap",
                    fontFamily: "monospace",
                  }}
                >
                  {describeParams(event.tool, event.params)}
                </div>
              )}
            </div>
            <span
              style={{
                color: "#999",
                fontSize: "0.7rem",
                flexShrink: 0,
                whiteSpace: "nowrap",
              }}
            >
              {timeAgo(event.timestamp)}
            </span>
          </div>
        ))}
      </div>
    </section>
  );
}
