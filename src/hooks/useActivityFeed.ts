import { useEffect, useState, useCallback } from "react";
import { listen } from "@tauri-apps/api/event";

export interface ActivityEvent {
  id: string;
  tool: string;
  params: Record<string, unknown>;
  status: "pending" | "success" | "error";
  timestamp: number;
}

const MAX_EVENTS = 20;

/**
 * Listen for tool request activity events from the Rust backend.
 * Shows real-time file access activity as it happens.
 */
export function useActivityFeed() {
  const [events, setEvents] = useState<ActivityEvent[]>([]);

  const addEvent = useCallback((event: ActivityEvent) => {
    setEvents((prev) => [event, ...prev].slice(0, MAX_EVENTS));
  }, []);

  const updateEvent = useCallback(
    (id: string, status: "success" | "error") => {
      setEvents((prev) =>
        prev.map((e) => (e.id === id ? { ...e, status } : e))
      );
    },
    []
  );

  useEffect(() => {
    const unlistenStart = listen<{
      request_id: string;
      tool: string;
      params: Record<string, unknown>;
    }>("tool:request_started", (event) => {
      addEvent({
        id: event.payload.request_id,
        tool: event.payload.tool,
        params: event.payload.params,
        status: "pending",
        timestamp: Date.now(),
      });
    });

    const unlistenEnd = listen<{
      request_id: string;
      status: "success" | "error";
    }>("tool:request_completed", (event) => {
      updateEvent(event.payload.request_id, event.payload.status);
    });

    return () => {
      unlistenStart.then((fn) => fn());
      unlistenEnd.then((fn) => fn());
    };
  }, [addEvent, updateEvent]);

  return events;
}
