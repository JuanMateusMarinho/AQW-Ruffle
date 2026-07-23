const { app, BrowserWindow, ipcMain, Menu, shell } = require("electron");
const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");

// Loader3 fetches the current game version and initializes Game.params before startup.
const AQW_SWF_URL = "https://game.aq.com/game/gamefiles/Loader3.swf";
const AQW_BASE_URL = "https://game.aq.com/game/gamefiles/";
const DRAGON_FABLE_SWF_URL = "https://play.dragonfable.com/game/DFLoader.swf";
const DRAGON_FABLE_BASE_URL = "https://play.dragonfable.com/game/";

function isMediaUrl(url) {
  try {
    const parsed = new URL(url);
    const host = parsed.hostname.replace(/^www\./, "");
    return (
      parsed.protocol === "https:"
      && (
        host === "youtube.com"
        || host === "youtu.be"
        || host.endsWith(".youtube.com")
        || host === "twitch.tv"
        || host.endsWith(".twitch.tv")
      )
    );
  } catch {
    return false;
  }
}

function resolveRuffleExe() {
  const candidates = [
    path.join(process.resourcesPath || "", "bin", "AQW.exe"),
    path.join(__dirname, "..", "..", "release", "AQW.exe"),
    path.join(__dirname, "..", "..", "target", "release", "ruffle_desktop.exe"),
  ];
  return candidates.find((candidate) => candidate && fs.existsSync(candidate));
}

function launchFlashGame({ swfUrl, baseUrl, title }) {
  const exe = resolveRuffleExe();
  if (!exe) {
    throw new Error("Could not find AQW.exe or ruffle_desktop.exe.");
  }

  const child = spawn(
    exe,
    [
      swfUrl,
      "--spoof-url",
      swfUrl,
      "--base",
      baseUrl,
      "--graphics",
      "vulkan",
      "--quality",
      "low",
      "--power",
      "high",
      "--frame-rate",
      "24",
      "--scale",
      "show-all",
      "--letterbox",
      "on",
      "--upgrade-to-https",
      "--player-version",
      "32",
      "-m",
      "60",
      "--no-gui",
      "--tcp-connections",
      "allow",
    ],
    {
      detached: true,
      stdio: "ignore",
      windowsHide: true,
      env: {
        ...process.env,
        ARTIX_RUFFLE_WINDOW_TITLE: title,
        RUST_LOG: "warn",
      },
    },
  );
  child.unref();
}

function openMediaWindow(url) {
  if (!isMediaUrl(url)) {
    return shell.openExternal(url);
  }

  const mediaWindow = new BrowserWindow({
    width: 1180,
    height: 720,
    minWidth: 860,
    minHeight: 520,
    backgroundColor: "#050507",
    title: "AdventureQuest Worlds Media",
    icon: path.join(__dirname, "icon.ico"),
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      webviewTag: false,
    },
  });

  mediaWindow.setMenu(null);
  mediaWindow.removeMenu();
  mediaWindow.loadURL(url);
  return null;
}

function createWindow() {
  const win = new BrowserWindow({
    width: 1180,
    height: 760,
    minWidth: 960,
    minHeight: 620,
    backgroundColor: "#090910",
    title: "Artix Games Launcher",
    icon: path.join(__dirname, "icon.ico"),
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      nodeIntegration: false,
      webviewTag: true,
      sandbox: false,
    },
  });

  win.setMenu(null);
  win.removeMenu();
  win.loadFile(path.join(__dirname, "renderer", "index.html"));
}

app.commandLine.appendSwitch("ignore-gpu-blocklist");
app.commandLine.appendSwitch("autoplay-policy", "no-user-gesture-required");

app.whenReady().then(() => {
  Menu.setApplicationMenu(null);
  createWindow();
});

app.on("browser-window-created", (_event, window) => {
  window.setMenu(null);
  window.removeMenu();
});

app.on("web-contents-created", (_event, contents) => {
  if (typeof contents.setWindowOpenHandler !== "function") return;

  contents.setWindowOpenHandler(({ url }) => {
    if (url && url !== "about:blank") {
      Promise.resolve(openMediaWindow(url)).catch(() => {});
    }
    return { action: "deny" };
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});

ipcMain.handle("launch-game", async (_event, game) => {
  if (game === "aqw") {
    launchFlashGame({
      swfUrl: AQW_SWF_URL,
      baseUrl: AQW_BASE_URL,
      title: "Artix Entertainment - AdventureQuest Worlds V2.2",
    });
    return { ok: true, message: "AdventureQuest Worlds started through Ruffle." };
  }
  if (game === "df") {
    launchFlashGame({
      swfUrl: DRAGON_FABLE_SWF_URL,
      baseUrl: DRAGON_FABLE_BASE_URL,
      title: "Artix Entertainment -Dragon Fable",
    });
    return { ok: true, message: "DragonFable started through Ruffle." };
  }
  return { ok: false, message: "This game is reserved for a future launcher update." };
});

ipcMain.handle("open-url", async (_event, url) => {
  await shell.openExternal(url);
  return { ok: true };
});

ipcMain.handle("open-media-window", async (_event, url) => {
  await openMediaWindow(url);
  return { ok: true };
});
