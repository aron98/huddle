// Huddle desktop shell — main process.
//
// The web client is deliberately same-origin (relative-path REST,
// scheme-matched signaling), so the shell cannot serve crates/web/dist itself.
// Instead it loads the HTTP origin of a Huddle server, which serves the
// identical built client (ADR 004, rust-embed). Dev mode points at a
// locally running `cargo run -p huddle-server`; production uses the
// user-configured server URL, collected by the first-run connect page and
// persisted to settings.json in userData (see settings.mjs).

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { app, BrowserWindow, ipcMain, Menu, Tray } from "electron";
import { parseDeepLink } from "./deep-link.mjs";
import { registerScreenShare } from "./screenshare.mjs";
import { registerUpdater } from "./updater.mjs";
import { loadSettings, normalizeServerUrl, saveSettings, settingsPath } from "./settings.mjs";
import { describeLoadError } from "./load-errors.mjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const isDev = process.argv.includes("--dev");
const DEV_SERVER = "http://localhost:3000";

let mainWindow = null;
let tray = null;
let quitting = false;
let settings = { serverUrl: null };
let connectAttemptUrl = null; // origin a save-server-url loadURL is trying
const queuedDeepLinks = [];

// Single instance: the second launch just routes its deep link to us.
if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on("second-instance", (_event, argv) => {
    focusMainWindow();
    const link = argv.find((a) => a.startsWith("huddle://"));
    if (link) handleDeepLink(link);
  });

  // macOS delivers deep links via open-url, possibly before ready.
  app.on("open-url", (event, link) => {
    event.preventDefault();
    if (mainWindow) handleDeepLink(link);
    else queuedDeepLinks.push(link);
  });

  app.whenReady().then(start);
}

async function start() {
  settings = loadSettings(fs, settingsPath(app.getPath("userData")));
  createWindow();
  registerIpc();

  if (app.isPackaged) app.setAsDefaultProtocolClient("huddle");
  setupTray();
  registerUpdater(); // self-update on packaged builds (no-op in dev) — issue #98

  while (queuedDeepLinks.length > 0) handleDeepLink(queuedDeepLinks.shift());
}

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 1280,
    height: 800,
    minWidth: 960,
    minHeight: 600,
    icon: path.join(__dirname, "..", "build", "icon.png"),
    autoHideMenuBar: process.platform !== "darwin",
    title: "Huddle",
    show: false,
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      sandbox: true,
      nodeIntegration: false,
    },
  });

  mainWindow.once("ready-to-show", () => mainWindow.show());
  mainWindow.on("close", (event) => {
    // Close-to-tray where a tray exists; real quit goes through before-quit.
    if (tray && !quitting) {
      event.preventDefault();
      mainWindow.hide();
    }
  });
  mainWindow.on("closed", () => {
    mainWindow = null;
  });

  // The client only ever needs camera/mic/screen, fullscreen and pointer
  // lock; everything else is denied by default.
  const allowedPermissions = new Set(["media", "fullscreen", "pointerLock"]);
  mainWindow.webContents.session.setPermissionRequestHandler(
    (_contents, permission, callback) => callback(allowedPermissions.has(permission)),
  );

  // Single-window app: no popups. External links stay in the OS browser of
  // whatever opened them; the meeting client opens nothing itself.
  mainWindow.webContents.setWindowOpenHandler(() => ({ action: "deny" }));

  // A configured server that cannot be reached drops back to the connect
  // page with the error, so the user is never stuck on a dead error page.
  mainWindow.webContents.on("did-fail-load", (_event, code, desc, url, isMainFrame) => {
    if (!isMainFrame || !mainWindow) return;
    if (url.startsWith("file://")) return; // our own pages already render
    if (code === -3) return; // ERR_ABORTED: we navigated elsewhere ourselves
    // A failed Connect submit is surfaced inline by the save-server-url
    // handler itself; reloading the page here would wipe that error and
    // replace it with Chromium's raw description. Consume the attempt once.
    if (connectAttemptUrl && url === connectAttemptUrl) {
      connectAttemptUrl = null;
      return;
    }
    console.error(`load failed (${code} ${desc}): ${url}`);
    showConnectPage(desc || `Could not reach the server (code ${code}).`);
  });

  registerScreenShare(mainWindow);
  openInitialPage();
}

function openInitialPage() {
  const target = isDev ? DEV_SERVER : settings.serverUrl;
  if (target) mainWindow.loadURL(target).catch((err) => console.error(err));
  else showConnectPage();
}

function showConnectPage(error) {
  if (!mainWindow) return;
  const options = error ? { search: new URLSearchParams({ error }).toString() } : {};
  mainWindow.loadFile(path.join(__dirname, "connect.html"), options).catch((err) =>
    console.error(err),
  );
}

function focusMainWindow() {
  if (!mainWindow) {
    createWindow();
    return;
  }
  if (mainWindow.isMinimized()) mainWindow.restore();
  mainWindow.show();
  mainWindow.focus();
}

function registerIpc() {
  ipcMain.handle("huddle:get-info", () => ({
    electron: process.versions.electron,
    chrome: process.versions.chrome,
    node: process.versions.node,
    platform: process.platform,
  }));
  ipcMain.handle("huddle:get-server-url", () => settings.serverUrl);
  ipcMain.handle("huddle:save-server-url", async (_event, raw) => {
    const normalized = normalizeServerUrl(raw);
    if (!normalized.ok) return normalized;
    settings = { serverUrl: normalized.url };
    saveSettings(fs, settingsPath(app.getPath("userData")), settings);
    // Await the load and report failures through the returned error object:
    // the connect page is still showing and renders it inline (connect.js).
    connectAttemptUrl = normalized.url;
    try {
      await mainWindow.loadURL(normalized.url);
      connectAttemptUrl = null;
      return { ok: true };
    } catch (err) {
      // Left set on failure: did-fail-load consumes it (see createWindow) so
      // the page is not reloaded out from under the inline error returned
      // here — whichever of the two fires first.
      console.error(`connect failed for ${normalized.url}:`, err);
      return { ok: false, error: describeLoadError(err) };
    }
  });
}

function handleDeepLink(link) {
  const parsed = parseDeepLink(link, settings.serverUrl);
  if (!parsed) {
    console.warn(`ignoring malformed deep link: ${link}`);
    return;
  }
  if (settings.serverUrl !== parsed.serverUrl) {
    settings = { serverUrl: parsed.serverUrl };
    saveSettings(fs, settingsPath(app.getPath("userData")), settings);
  }
  focusMainWindow();
  mainWindow.loadURL(parsed.serverUrl + parsed.path).catch((err) => console.error(err));
}

function setupTray() {
  // macOS keeps the app in the Dock; a tray there would be duplicate UI.
  if (process.platform === "darwin") return;
  try {
    tray = new Tray(path.join(__dirname, "..", "build", "trayTemplate.png"));
    tray.setToolTip("Huddle");
    tray.setContextMenu(
      Menu.buildFromTemplate([
        { label: "Show Huddle", click: focusMainWindow },
        { type: "separator" },
        {
          label: "Quit",
          click: () => {
            quitting = true;
            app.quit();
          },
        },
      ]),
    );
    tray.on("click", focusMainWindow);
  } catch (err) {
    // Common on minimal Linux WMs/DEs without a system tray.
    console.warn(`tray unavailable: ${err.message}`);
    tray = null;
  }
}

app.on("before-quit", () => {
  quitting = true;
});

app.on("window-all-closed", () => {
  // macOS convention: keep running in the Dock, activate reopens the window.
  if (process.platform !== "darwin") app.quit();
});

app.on("activate", () => {
  focusMainWindow();
});
