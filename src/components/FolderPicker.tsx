import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";

export default function FolderPicker() {
  const [folders, setFolders] = useState<string[]>([]);

  useEffect(() => {
    invoke<string[]>("get_scoped_folders").then(setFolders);
  }, []);

  const addFolder = async () => {
    const selected = await open({ directory: true, multiple: false });
    if (selected && typeof selected === "string") {
      const updated = [...folders, selected];
      setFolders(updated);
      await invoke("set_scoped_folders", { folders: updated });
    }
  };

  const removeFolder = async (index: number) => {
    const updated = folders.filter((_, i) => i !== index);
    setFolders(updated);
    await invoke("set_scoped_folders", { folders: updated });
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
          onClick={addFolder}
          style={{
            fontSize: "0.8rem",
            padding: "0.25rem 0.75rem",
            border: "1px solid #ddd",
            borderRadius: 6,
            background: "white",
            cursor: "pointer",
          }}
        >
          + Add Folder
        </button>
      </div>

      <p style={{ color: "#666", fontSize: "0.8rem", marginBottom: "0.75rem" }}>
        The AI agent can only access files within these folders.
      </p>

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
                style={{
                  fontSize: "0.75rem",
                  padding: "0.15rem 0.5rem",
                  border: "1px solid #e0e0e0",
                  borderRadius: 4,
                  background: "white",
                  color: "#c00",
                  cursor: "pointer",
                  flexShrink: 0,
                }}
              >
                Remove
              </button>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
