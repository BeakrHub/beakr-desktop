import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";

// Shown once, before the user grants the very first folder, so it is clear that
// Beakr reads file contents (the deny-list only blocks secret-looking filenames,
// not secrets written inside otherwise-ordinary files). Persisted so it never
// nags again after it has been acknowledged.
const ACCESS_NOTE_ACK_KEY = "beakr-folder-access-acknowledged";

export default function FolderPicker() {
  const [folders, setFolders] = useState<string[]>([]);
  const [showAccessNote, setShowAccessNote] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    invoke<string[]>("get_scoped_folders")
      .then(setFolders)
      .catch((e) => {
        setError(typeof e === "string" ? e : "Could not load allowed folders.");
      });
  }, []);

  // Open the native picker and persist the new folder. Separated from the click
  // handler so the one-time note can gate the first call without duplicating it.
  const pickAndAddFolder = async () => {
    setError(null);
    try {
      const selected = await open({ directory: true, multiple: false });
      if (!selected || typeof selected !== "string") return;
      if (folders.includes(selected)) return;

      const updated = [...folders, selected];
      setSaving(true);
      await invoke("set_scoped_folders", { folders: updated });
      setFolders(updated);
    } catch (e) {
      setError(typeof e === "string" ? e : "Could not update allowed folders.");
    } finally {
      setSaving(false);
    }
  };

  const handleAddFolderClick = async () => {
    const acknowledged = localStorage.getItem(ACCESS_NOTE_ACK_KEY);
    if (!acknowledged) {
      setShowAccessNote(true);
      return;
    }
    await pickAndAddFolder();
  };

  const acknowledgeAndAdd = async () => {
    localStorage.setItem(ACCESS_NOTE_ACK_KEY, "1");
    setShowAccessNote(false);
    await pickAndAddFolder();
  };

  const removeFolder = async (index: number) => {
    setError(null);
    const updated = folders.filter((_, i) => i !== index);
    try {
      setSaving(true);
      await invoke("set_scoped_folders", { folders: updated });
      setFolders(updated);
    } catch (e) {
      setError(typeof e === "string" ? e : "Could not remove folder.");
    } finally {
      setSaving(false);
    }
  };

  return (
    <section style={{ marginTop: "1.5rem" }}>
      <div
        style={{
          display: "flex",
          justifyContent: "space-between",
          alignItems: "center",
          marginBottom: "0.75rem",
        }}
      >
        <h2 style={{ fontSize: "1rem", fontWeight: 600, margin: 0 }}>
          Allowed Folders
        </h2>
        <button
          onClick={handleAddFolderClick}
          disabled={saving}
          style={{
            fontSize: "0.8rem",
            padding: "0.25rem 0.75rem",
            border: "1px solid #ddd",
            borderRadius: 6,
            background: "white",
            cursor: saving ? "default" : "pointer",
            opacity: saving ? 0.6 : 1,
          }}
        >
          + Add Folder
        </button>
      </div>

      <p style={{ color: "#666", fontSize: "0.8rem", marginBottom: "0.75rem" }}>
        The AI agent can only access files within these folders.
      </p>

      {error && (
        <p
          role="alert"
          style={{ color: "#dc2626", fontSize: "0.8rem", marginBottom: "0.75rem" }}
        >
          {error}
        </p>
      )}

      {folders.length === 0 ? (
        <div
          style={{
            padding: "2rem",
            textAlign: "center",
            color: "#999",
            border: "1px dashed #ddd",
            borderRadius: 8,
            fontSize: "0.85rem",
          }}
        >
          No folders added yet. Click "Add Folder" to grant access.
        </div>
      ) : (
        <ul style={{ listStyle: "none", padding: 0, margin: 0 }}>
          {folders.map((folder, i) => (
            <li
              key={folder}
              style={{
                display: "flex",
                justifyContent: "space-between",
                alignItems: "center",
                padding: "0.5rem 0.75rem",
                borderBottom: "1px solid #eee",
                fontSize: "0.85rem",
              }}
            >
              <span
                style={{
                  overflow: "hidden",
                  textOverflow: "ellipsis",
                  whiteSpace: "nowrap",
                  flex: 1,
                  marginRight: "0.5rem",
                  fontFamily: "monospace",
                  fontSize: "0.8rem",
                }}
              >
                {folder}
              </span>
              <button
                onClick={() => removeFolder(i)}
                disabled={saving}
                style={{
                  fontSize: "0.75rem",
                  padding: "0.15rem 0.5rem",
                  border: "1px solid #e0e0e0",
                  borderRadius: 4,
                  background: "white",
                  color: "#c00",
                  cursor: saving ? "default" : "pointer",
                  opacity: saving ? 0.6 : 1,
                  flexShrink: 0,
                }}
              >
                Remove
              </button>
            </li>
          ))}
        </ul>
      )}

      {showAccessNote && (
        <div
          role="dialog"
          aria-modal="true"
          aria-labelledby="folder-access-note-title"
          style={{
            position: "fixed",
            inset: 0,
            background: "rgba(0, 0, 0, 0.35)",
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            padding: "1.5rem",
            zIndex: 1000,
          }}
        >
          <div
            style={{
              background: "white",
              borderRadius: 12,
              padding: "1.25rem 1.25rem 1rem",
              maxWidth: 360,
              width: "100%",
              boxShadow: "0 12px 32px rgba(0, 0, 0, 0.18)",
            }}
          >
            <h3
              id="folder-access-note-title"
              style={{ margin: "0 0 0.5rem", fontSize: "1rem", fontWeight: 600 }}
            >
              Before you add a folder
            </h3>
            <p
              style={{
                margin: "0 0 1rem",
                fontSize: "0.85rem",
                lineHeight: 1.5,
                color: "#444",
              }}
            >
              Beakr can read any file inside the folders you add, including files
              that contain passwords, API keys, or other secrets. Only add folders
              you are comfortable letting the AI read.
            </p>
            <div
              style={{
                display: "flex",
                justifyContent: "flex-end",
                gap: "0.5rem",
              }}
            >
              <button
                onClick={() => setShowAccessNote(false)}
                style={{
                  fontSize: "0.8rem",
                  padding: "0.35rem 0.85rem",
                  border: "1px solid #ddd",
                  borderRadius: 6,
                  background: "white",
                  cursor: "pointer",
                }}
              >
                Cancel
              </button>
              <button
                onClick={acknowledgeAndAdd}
                style={{
                  fontSize: "0.8rem",
                  padding: "0.35rem 0.85rem",
                  border: "1px solid #2563eb",
                  borderRadius: 6,
                  background: "#2563eb",
                  color: "white",
                  cursor: "pointer",
                }}
              >
                Choose folder
              </button>
            </div>
          </div>
        </div>
      )}
    </section>
  );
}
