// Screen sharing for the desktop shell.
//
// The web client calls getDisplayMedia() (video always, audio unless the
// ?screen-audio=0 knob is set). The shell replaces Chromium's built-in
// picker with its own modal window over desktopCapturer sources, matching
// what a plain Chrome tab shows. Cancelling the picker invokes the
// callback with no source so getDisplayMedia rejects with NotAllowedError,
// exactly like cancelling Chrome's native dialog.
//
// Loopback system audio ("share computer audio") is only offered where
// Electron supports the 'loopback' audio source: Windows any version,
// macOS >= 13.3. On Linux the picker shows video-only sources, and
// getDisplayMedia still resolves without an audio track.

import path from "node:path";
import { BrowserWindow, desktopCapturer, ipcMain } from "electron";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

const PICKER_WIDTH = 720;
const PICKER_HEIGHT = 520;

/** @returns {boolean} whether `audio: 'loopback'` works on this machine */
export function supportsLoopbackAudio() {
  if (process.platform === "win32") return true;
  if (process.platform === "darwin") {
    const parts = (typeof process.getSystemVersion === "function"
      ? process.getSystemVersion()
      : ""
    ).split(".");
    const [major = 0, minor = 0] = parts.map((n) => Number.parseInt(n, 10));
    return major > 13 || (major === 13 && minor >= 3);
  }
  return false;
}

function closePicker(picker) {
  try {
    if (!picker.isDestroyed()) picker.destroy();
  } catch {
    // Already gone.
  }
}

/**
 * Installs the display-media handler on the main window's session.
 * One picker at a time; a second concurrent request is rejected (callback
 * with no arguments), like clicking Cancel on the native dialog.
 */
export function registerScreenShare(mainWindow) {
  const loopback = supportsLoopbackAudio();
  let activePicker = false;

  mainWindow.webContents.session.setDisplayMediaRequestHandler(
    (request, callback) => {
      // If anything throws (getSources, window creation), settle the
      // request like a cancelled picker instead of hanging getDisplayMedia.
      runPicker(request, callback).catch(() => {
        activePicker = false;
        callback();
      });
    },
  );

  async function runPicker(request, callback) {
    // One picker at a time; a concurrent request reads as "user cancelled".
    if (activePicker) {
      callback();
      return;
    }
    activePicker = true;

    let settled = false;
    const sources = await desktopCapturer.getSources({
      types: ["screen", "window"],
      thumbnailSize: { width: 320, height: 180 },
    });

    let picker = null;
    const onReady = () => {
      if (!picker || picker.isDestroyed()) return;
      picker.webContents.send("huddle-picker:sources", {
        sources: sources.map((source) => ({
          id: source.id,
          name: source.name,
          thumbnail: source.thumbnail.toDataURL(),
        })),
        audioSupported: loopback,
      });
    };
    const onSelect = (_event, { id, withAudio }) => {
      const source = sources.find((s) => s.id === id);
      if (!source) {
        finish(undefined);
        return;
      }
      const wantsAudio = Boolean(request.audio) && loopback && Boolean(withAudio);
      finish(wantsAudio ? { video: source, audio: "loopback" } : { video: source });
    };
    const onCancel = () => finish(undefined);

    // Must be called exactly once; a bare callback() rejects getDisplayMedia
    // with NotAllowedError, matching Chrome's native-cancel behaviour.
    const finish = (result) => {
      if (settled) return;
      settled = true;
      activePicker = false;
      ipcMain.removeListener("huddle-picker:ready", onReady);
      ipcMain.removeListener("huddle-picker:select", onSelect);
      ipcMain.removeListener("huddle-picker:cancel", onCancel);
      closePicker(picker);
      callback(result);
    };

    picker = new BrowserWindow({
      parent: mainWindow,
      modal: true,
      width: PICKER_WIDTH,
      height: PICKER_HEIGHT,
      resizable: false,
      minimizable: false,
      maximizable: false,
      show: false,
      autoHideMenuBar: true,
      title: "Choose what to share",
      webPreferences: {
        preload: path.join(__dirname, "picker-preload.js"),
        contextIsolation: true,
        sandbox: true,
        nodeIntegration: false,
      },
    });

    picker.on("closed", () => finish(undefined));
    picker.once("ready-to-show", () => {
      if (!picker.isDestroyed()) picker.show();
    });
    ipcMain.on("huddle-picker:ready", onReady);
    ipcMain.on("huddle-picker:select", onSelect);
    ipcMain.on("huddle-picker:cancel", onCancel);

    await picker.loadFile(path.join(__dirname, "picker.html"));
  }
}
