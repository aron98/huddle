// Connect-page preload: exposes just the submit call.
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("huddleConnect", {
  submit: (serverUrl) => ipcRenderer.invoke("huddle:save-server-url", serverUrl),
});
