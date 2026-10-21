// Picker preload: sandboxed bridge to the main-side screen-share flow.
const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("huddlePicker", {
  ready: () => ipcRenderer.send("huddle-picker:ready"),
  select: (id, withAudio) => ipcRenderer.send("huddle-picker:select", { id, withAudio }),
  cancel: () => ipcRenderer.send("huddle-picker:cancel"),
  onSources: (callback) =>
    ipcRenderer.on("huddle-picker:sources", (_event, payload) => callback(payload)),
});
