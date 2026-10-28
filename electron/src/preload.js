// Main-window preload: sandbox-compatible CJS (package.json has no "type",
// so .js files load as plain scripts). Exposes a minimal, promise-based API.
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("huddleDesktop", {
  /** Shell/runtime versions — used by the manual verification checklist. */
  info: () => ipcRenderer.invoke("huddle:get-info"),
  getServerUrl: () => ipcRenderer.invoke("huddle:get-server-url"),
  saveServerUrl: (url) => ipcRenderer.invoke("huddle:save-server-url", url),
});

// The first-run connect page (connect.html) runs in THIS window — the shell
// is single-window by design (ADR 014) — so its bridge must come from this
// preload. The dedicated connect-preload.js was never attached to any
// BrowserWindow, leaving window.huddleConnect undefined and the connect
// form dead in every shipped build (issue #107).
contextBridge.exposeInMainWorld("huddleConnect", {
  submit: (serverUrl) => ipcRenderer.invoke("huddle:save-server-url", serverUrl),
});
