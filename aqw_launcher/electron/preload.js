const { contextBridge, ipcRenderer } = require("electron");

contextBridge.exposeInMainWorld("launcher", {
  launchGame: (game) => ipcRenderer.invoke("launch-game", game),
  openUrl: (url) => ipcRenderer.invoke("open-url", url),
  openMediaWindow: (url) => ipcRenderer.invoke("open-media-window", url),
});
