const statusText = document.getElementById("status");

function setStatus(message) {
  if (statusText) statusText.textContent = message;
}

function setTab(tab) {
  document.querySelectorAll(".top-tab").forEach((button) => {
    button.classList.toggle("active", button.dataset.tab === tab);
  });
  document.querySelectorAll(".tab-panel").forEach((panel) => {
    panel.classList.toggle("active", panel.id === `tab-${tab}`);
  });
}

document.querySelectorAll(".top-tab").forEach((button) => {
  button.addEventListener("click", () => setTab(button.dataset.tab));
});

document.getElementById("play-aqw").addEventListener("click", async () => {
  window.dispatchEvent(new CustomEvent("aqw:play-clicked"));
  setStatus("Starting AdventureQuest Worlds...");
  const result = await window.launcher.launchGame("aqw");
  setStatus(result.message || "Ready to play.");
});

document.querySelectorAll(".side-item").forEach((button) => {
  button.addEventListener("click", async () => {
    const game = button.dataset.game;
    if (game === "aqw") {
      document.querySelectorAll(".side-item").forEach((item) => item.classList.remove("active"));
      button.classList.add("active");
      setTab("games");
      return;
    }
    if (game === "df") {
      setStatus("Starting DragonFable...");
      const result = await window.launcher.launchGame("df");
      setStatus(result.message || "Ready to play.");
      return;
    }
    setStatus("This game is reserved for a future launcher update.");
  });
});

document.querySelectorAll("[data-open]").forEach((button) => {
  button.addEventListener("click", () => {
    window.launcher.openUrl(button.dataset.open);
  });
});

document.querySelectorAll(".source-tab").forEach((button) => {
  button.addEventListener("click", () => {
    const source = button.dataset.liveSource;
    document.querySelectorAll(".source-tab").forEach((tab) => {
      tab.classList.toggle("active", tab.dataset.liveSource === source);
    });
    document.querySelectorAll(".live-webview").forEach((view) => {
      view.classList.toggle("active", view.id === `live-${source}`);
    });
  });
});

function getWebviewTarget(target) {
  if (target === "active-live") {
    return document.querySelector(".live-webview.active");
  }
  return document.getElementById(target);
}

document.querySelectorAll("[data-web-action]").forEach((button) => {
  button.addEventListener("click", () => {
    const webview = getWebviewTarget(button.dataset.webTarget);
    if (!webview) return;

    if (button.dataset.webAction === "back") {
      if (webview.canGoBack && webview.canGoBack()) {
        webview.goBack();
        return;
      }
      if (webview.dataset.homeUrl) {
        webview.loadURL(webview.dataset.homeUrl);
      }
      return;
    }

    if (button.dataset.webAction === "home" && webview.dataset.homeUrl) {
      webview.loadURL(webview.dataset.homeUrl);
    }
  });
});

function normalizeUrl(url) {
  try {
    const parsed = new URL(url);
    parsed.hash = "";
    return parsed.toString().replace(/\/$/, "");
  } catch {
    return url;
  }
}

function isAllowedInLauncher(url, homeUrl) {
  try {
    const current = new URL(url);
    const home = new URL(homeUrl);
    if (current.hostname !== home.hostname) return false;

    const currentClean = normalizeUrl(url);
    const homeClean = normalizeUrl(homeUrl);
    if (currentClean === homeClean) return true;

    if (home.hostname.includes("youtube.com")) {
      const allowedPrefixes = [
        home.pathname.replace(/\/recent$/, ""),
        home.pathname.replace(/\/live$/, ""),
      ].filter(Boolean);
      return allowedPrefixes.some((prefix) => current.pathname.startsWith(prefix))
        && !current.pathname.startsWith("/watch")
        && !current.pathname.startsWith("/shorts");
    }

    if (home.hostname.includes("twitch.tv")) {
      return current.pathname.startsWith("/directory/category/adventurequest-worlds");
    }

    return false;
  } catch {
    return false;
  }
}

function openMediaFromWebview(webview, url) {
  const homeUrl = webview.dataset.homeUrl;
  if (!url || url === "about:blank") return;
  if (isAllowedInLauncher(url, homeUrl)) return;

  window.launcher.openMediaWindow(url);
  setTimeout(() => {
    if (webview.getURL && !isAllowedInLauncher(webview.getURL(), homeUrl)) {
      webview.loadURL(homeUrl);
    }
  }, 60);
}

document.querySelectorAll(".external-click-webview").forEach((webview) => {
  webview.addEventListener("dom-ready", () => {
    webview.executeJavaScript(`
      if (!window.__aqwLauncherMediaIntercept) {
        window.__aqwLauncherMediaIntercept = true;
        const mediaPattern = /youtube\\.com\\/(watch|shorts|live)|youtu\\.be\\/|twitch\\.tv\\/(?!directory\\/category\\/adventurequest-worlds)/i;
        const findAnchor = (event) => {
          const path = typeof event.composedPath === 'function' ? event.composedPath() : [];
          for (const node of path) {
            if (!node || node === window || node === document) continue;
            if (node.href && node.tagName === 'A') return node;
            if (typeof node.closest === 'function') {
              const anchor = node.closest('a[href]');
              if (anchor) return anchor;
            }
          }
          return event.target && event.target.closest ? event.target.closest('a[href]') : null;
        };

        window.addEventListener('click', (event) => {
          const anchor = findAnchor(event);
          if (!anchor) return;
          const href = anchor.href;
          if (!mediaPattern.test(href)) return;

          event.preventDefault();
          event.stopPropagation();
          event.stopImmediatePropagation();
          window.open(href, '_blank', 'noopener,noreferrer');
          return false;
        }, true);

        window.addEventListener('auxclick', (event) => {
          const anchor = findAnchor(event);
          if (!anchor) return;
          const href = anchor.href;
          if (!mediaPattern.test(href)) return;

          event.preventDefault();
          event.stopPropagation();
          event.stopImmediatePropagation();
          window.open(href, '_blank', 'noopener,noreferrer');
          return false;
        }, true);
      }
    `).catch(() => {});
  });

  webview.addEventListener("will-navigate", (event) => {
    if (!isAllowedInLauncher(event.url, webview.dataset.homeUrl)) {
      event.preventDefault();
      window.launcher.openMediaWindow(event.url);
    }
  });

  webview.addEventListener("new-window", (event) => {
    event.preventDefault();
    openMediaFromWebview(webview, event.url);
  });

  webview.addEventListener("did-navigate", (event) => {
    openMediaFromWebview(webview, event.url);
  });

  webview.addEventListener("did-navigate-in-page", (event) => {
    openMediaFromWebview(webview, event.url);
  });
});
