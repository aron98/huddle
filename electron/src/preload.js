// Main-window preload: sandbox-compatible CJS (package.json has no "type",
// so .js files load as plain scripts). Exposes a minimal, promise-based API.
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("huddleDesktop", {
  /** Shell/runtime versions — used by the manual verification checklist. */
  info: () => ipcRenderer.invoke("huddle:get-info"),
  getServerUrl: () => ipcRenderer.invoke("huddle:get-server-url"),
  saveServerUrl: (url) => ipcRenderer.invoke("huddle:save-server-url", url),
});
