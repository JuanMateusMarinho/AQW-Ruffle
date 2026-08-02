param(
    [string]$ArtixLauncherPath = "C:\Program Files\Artix Game Launcher",
    [string]$AqwExePath,
    [string]$OutputDir,
    [switch]$Install
)

$ErrorActionPreference = "Stop"

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
if (-not $AqwExePath) {
    $AqwExePath = Join-Path $repoRoot "release\AQW.exe"
}
if (-not $OutputDir) {
    $OutputDir = Join-Path $repoRoot "release\artix_official_patch"
}

$gameExePaths = [ordered]@{
    "AQW.exe" = $AqwExePath
    "EpicDuel.exe" = Join-Path $repoRoot "release\EpicDuel.exe"
    "DragonFable.exe" = Join-Path $repoRoot "release\DragonFable.exe"
    "AdventureQuest.exe" = Join-Path $repoRoot "release\AdventureQuest.exe"
    "MechQuest.exe" = Join-Path $repoRoot "release\MechQuest.exe"
    "OverSoul.exe" = Join-Path $repoRoot "release\OverSoul.exe"
}

$resourcesPath = Join-Path $ArtixLauncherPath "resources"
$sourceAsar = Join-Path $resourcesPath "app.asar"
$sourcePreload = Join-Path $resourcesPath "preload.js"
$patchedAsar = Join-Path $OutputDir "app.asar"
$patchedPreload = Join-Path $OutputDir "preload.js"
$patchedBinDir = Join-Path $OutputDir "bin"

if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
    throw "Node.js is required to patch app.asar."
}
if (-not (Test-Path -LiteralPath $sourceAsar)) {
    throw "Could not find Artix launcher app.asar at $sourceAsar"
}
if (-not (Test-Path -LiteralPath $sourcePreload)) {
    throw "Could not find Artix launcher preload.js at $sourcePreload"
}
foreach ($entry in $gameExePaths.GetEnumerator()) {
    if (-not (Test-Path -LiteralPath $entry.Value)) {
        throw "Could not find $($entry.Key) at $($entry.Value)"
    }
}

New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null
New-Item -ItemType Directory -Path $patchedBinDir -Force | Out-Null

$nodeScript = @'
const fs = require("fs");
const path = require("path");

const sourceAsar = process.argv[2];
const outputAsar = process.argv[3];

const asar = fs.readFileSync(sourceAsar);
const headerBufLength = asar.readUInt32LE(4);
const headerStringLength = asar.readUInt32LE(12);
const header = JSON.parse(asar.slice(16, 16 + headerStringLength).toString("utf8"));
const dataBase = 8 + headerBufLength;

function collectFiles(node, prefix = "", result = []) {
  for (const name of Object.keys(node.files || {})) {
    const entry = node.files[name];
    const filePath = prefix ? `${prefix}/${name}` : name;
    if (entry.files) {
      collectFiles(entry, filePath, result);
    } else {
      result.push({ path: filePath, entry });
    }
  }
  return result;
}

function readOriginalFile(entry) {
  const start = dataBase + Number(entry.offset);
  return asar.slice(start, start + entry.size);
}

const files = collectFiles({ files: header.files });
const contents = new Map(files.map((file) => [file.path, readOriginalFile(file.entry)]));
let main = contents.get("main.js").toString("utf8");

let aqTubePatch = String.raw`
const aqTubeFs = require('fs');
const aqTubePath = require('path');
const aqTubeLegacyPartitions = [
	'artix-aqtube',
	'artix-aqtube-window',
	'artix-aqtube-window-en'
];

app.clearAqTubeLegacyStorage = () => {
	try {
		const userDataPath = app.getPath('userData');
		for (const partitionName of aqTubeLegacyPartitions) {
			aqTubeFs.rmSync(aqTubePath.join(userDataPath, 'Partitions', partitionName), {
				recursive: true,
				force: true
			});
		}
	} catch (error) {
		devToolsLog('AQTUBE legacy storage clear failed: ' + error.message);
	}
};

app.clearAqTubeLegacyStorage();

app.openAqTubeWindow = () => {
	const { BrowserView } = require('electron');
	const channelUrl = 'https://www.youtube.com/channel/UC0vYUqgESNR3sqEPiJ4SpeA?hl=en&gl=US';
	const toolbarHeight = 48;

	if (app.aqTubeWindow && !app.aqTubeWindow.isDestroyed()) {
		app.aqTubeWindow.show();
		app.aqTubeWindow.focus();
		app.aqTubeWindow.maximize();
		return;
	}

	const html = [
		'<!doctype html><html><head><meta charset="utf-8">',
		'<title>AQTUBE</title>',
		'<style>',
		'html,body{margin:0;width:100%;height:100%;overflow:hidden;background:#050505;color:#f5d09a;font-family:Arial,sans-serif;}',
		'.toolbar{height:48px;display:flex;align-items:center;gap:8px;padding:0 12px;background:linear-gradient(#211208,#070402);border-bottom:1px solid #7f3908;box-sizing:border-box;}',
		'.brand{font-weight:700;color:#fff;margin-right:8px;text-shadow:0 1px 2px #000;}',
		'button{height:30px;padding:0 12px;border:1px solid #6e4214;background:linear-gradient(#4a2b12,#170c06);color:#ffd7a0;font-weight:700;font-size:12px;cursor:pointer;text-transform:uppercase;}',
		'button:hover{border-color:#c56b16;color:#fff;}',
		'.spacer{flex:1;}',
		'</style></head><body>',
		'<div class="toolbar"><span class="brand">AQTUBE</span><button id="back">Back</button><button id="home">Channel</button><span class="spacer"></span><button id="open">Open Window</button></div>',
		'<script>',
		'const app=require("electron").remote.app;',
		'document.getElementById("back").addEventListener("click",()=>app.aqTubeGoBack());',
		'document.getElementById("home").addEventListener("click",()=>app.aqTubeGoHome());',
		'document.getElementById("open").addEventListener("click",()=>app.aqTubeOpenCurrent());',
		'</script>',
		'</body></html>'
	].join('');

	app.aqTubeWindow = new BrowserWindow({
		title: 'AQTUBE',
		width: 1280,
		height: 820,
		minWidth: 960,
		minHeight: 640,
		backgroundColor: '#000000',
		show: false,
		resizable: true,
		useContentSize: true,
		autoHideMenuBar: true,
		webPreferences: {
			nodeIntegration: true,
			contextIsolation: false,
			devTools: false
		}
	});

	app.aqTubeView = new BrowserView({
		webPreferences: {
			nodeIntegration: false,
			contextIsolation: true,
			devTools: false,
			partition: 'artix-aqtube-window'
		}
	});

	const aqTubeStorageTypes = [
		'cookies',
		'localstorage',
		'indexdb',
		'websql',
		'serviceworkers',
		'cachestorage',
		'filesystem',
		'appcache'
	];

	const clearAqTubeSession = () => {
		if (!app.aqTubeView || !app.aqTubeView.webContents) {
			return Promise.resolve();
		}
		const aqTubeSession = app.aqTubeView.webContents.session;
		if (!aqTubeSession) {
			return Promise.resolve();
		}
		return aqTubeSession.clearStorageData({ storages: aqTubeStorageTypes })
			.then(() => aqTubeSession.clearCache ? aqTubeSession.clearCache() : null)
			.catch((error) => {
				devToolsLog('AQTUBE storage clear failed: ' + error.message);
			});
	};

	const layoutAqTubeView = () => {
		if (!app.aqTubeWindow || app.aqTubeWindow.isDestroyed() || !app.aqTubeView) {
			return;
		}
		const size = app.aqTubeWindow.getContentSize();
		app.aqTubeView.setBounds({
			x: 0,
			y: toolbarHeight,
			width: size[0],
			height: Math.max(120, size[1] - toolbarHeight)
		});
		if (app.aqTubeView.setAutoResize) {
			app.aqTubeView.setAutoResize({ width: true, height: true });
		}
	};

	const loadAqTubeHome = () => {
		if (app.aqTubeView && app.aqTubeView.webContents) {
			app.aqTubeView.webContents.loadURL(channelUrl, {
				extraHeaders: 'Accept-Language: en-US,en;q=0.9\n'
			});
		}
	};

	app.aqTubeWindow.setBrowserView(app.aqTubeView);
	app.aqTubeWindow.on('resize', layoutAqTubeView);
	app.aqTubeWindow.on('maximize', layoutAqTubeView);
	app.aqTubeWindow.on('unmaximize', layoutAqTubeView);
	app.aqTubeWindow.on('close', () => {
		clearAqTubeSession();
	});
	app.aqTubeWindow.on('closed', () => {
		app.aqTubeWindow = null;
		app.aqTubeView = null;
	});

	app.aqTubeGoBack = () => {
		if (app.aqTubeView && app.aqTubeView.webContents && app.aqTubeView.webContents.canGoBack()) {
			app.aqTubeView.webContents.goBack();
			return;
		}
		loadAqTubeHome();
	};
	app.aqTubeGoHome = loadAqTubeHome;
	app.aqTubeOpenCurrent = () => {
		const url = app.aqTubeView && app.aqTubeView.webContents ? app.aqTubeView.webContents.getURL() : channelUrl;
		app.launchNewWindowURL(url || channelUrl, 1180, 760, true);
	};

	app.aqTubeView.webContents.on('new-window', (event, url) => {
		if (!url) {
			return;
		}
		event.preventDefault();
		app.launchNewWindowURL(url, 1180, 760, true);
	});

	app.aqTubeWindow.loadURL('data:text/html;charset=utf-8,' + encodeURIComponent(html));
	app.aqTubeWindow.once('ready-to-show', () => {
		layoutAqTubeView();
		app.aqTubeWindow.show();
		app.aqTubeWindow.maximize();
	});
	clearAqTubeSession().then(loadAqTubeHome);
};

app.injectAqTubeTab = () => {
	if (!mainWindow || !mainWindow.webContents || mainWindow.isDestroyed()) {
		return;
	}

	const script = String.raw` + "`" + `
(() => {
	function findTopNav() {
		return document.getElementById('topnav')
			|| document.querySelector('ul.topnav')
			|| document.querySelector('.topnav ul')
			|| document.querySelector('nav ul')
			|| document.querySelector('ul');
	}

	if (document.getElementById('aqtube-nav-item')) {
		return;
	}

	const nav = findTopNav();
	const navItem = document.createElement(nav && nav.tagName === 'UL' ? 'li' : 'span');
	navItem.id = 'aqtube-nav-item';
	navItem.style.cssText = 'display:inline-block;vertical-align:top;background:linear-gradient(#3d1f09,#140805);';

	const link = document.createElement('a');
	link.href = '#aqtube';
	link.textContent = 'AQTUBE';
	link.title = 'AQTubers';
	link.style.cssText = [
		'display:block',
		'height:44px',
		'line-height:44px',
		'padding:0 13px',
		'color:#f4e0bd',
		'font:700 15px Georgia,serif',
		'letter-spacing:0',
		'text-decoration:none',
		'text-shadow:0 1px 2px #000',
		'box-shadow:inset 0 -2px 0 #f08219'
	].join(';');

	link.addEventListener('click', (event) => {
		event.preventDefault();
		event.stopPropagation();
		if (window.interop && window.interop.openAqTubeWindow) {
			window.interop.openAqTubeWindow();
			return;
		}
		window.open('https://www.youtube.com/channel/UC0vYUqgESNR3sqEPiJ4SpeA', 'aqtube-window', 'top=90,left=80,width=1180,height=760');
	});

	navItem.appendChild(link);
	if (nav) {
		nav.appendChild(navItem);
	} else {
		navItem.style.cssText += 'position:fixed;z-index:2147483001;top:29px;left:442px;';
		document.body.appendChild(navItem);
	}
})();
` + "`" + `;

	mainWindow.webContents.executeJavaScript(script).catch((error) => {
		devToolsLog('AQTUBE injection failed: ' + error.message);
	});
};

`;

const insertBefore = "app.launchGameLocal = (gameName) => {";
const artixRuffleLauncher = String.raw`
app.artixRuffleGames = {
	aqw: {
		exe: 'AQW.exe',
		icon: 'aqw',
		title: 'Artix Entertainment - AdventureQuest Worlds V2.6',
		swfURL: 'https://game.aq.com/game/gamefiles/Loader3.swf',
		baseURL: 'https://game.aq.com/game/gamefiles/',
		width: '960',
		height: '580',
		graphics: 'vulkan'
	},
	ed: {
		exe: 'EpicDuel.exe',
		icon: 'epicduel',
		title: 'Artix Entertainment - Epic Duel',
		swfURL: 'https://epicduelstage.artix.com/omegaLoader14.swf',
		baseURL: 'https://epicduelstage.artix.com/',
		width: '1016',
		height: '700',
		graphics: 'vulkan'
	},
	df: {
		exe: 'DragonFable.exe',
		icon: 'dragonfable',
		title: 'Artix Entertainment - Dragon Fable',
		swfURL: 'https://play.dragonfable.com/game/dfloader.swf',
		baseURL: 'https://play.dragonfable.com/game/',
		width: '1016',
		height: '700',
		graphics: 'vulkan'
	},
	aq: {
		exe: 'AdventureQuest.exe',
		icon: 'adventurequest',
		title: 'Artix Entertainment - AdventureQuest',
		swfURL: 'https://aq.battleon.com/game/flash/Lore4652.swf',
		baseURL: 'https://aq.battleon.com/game/flash/',
		width: '800',
		height: '600',
		graphics: 'vulkan'
	},
	mq: {
		exe: 'MechQuest.exe',
		icon: 'mechquest',
		title: 'Artix Entertainment - MechQuest',
		swfURL: 'https://play.mechquest.com/game/gamefiles/MQLoader4.swf?isWeb=true',
		baseURL: 'https://play.mechquest.com/game/gamefiles/',
		width: '1016',
		height: '700',
		graphics: 'vulkan'
	},
	os: {
		exe: 'OverSoul.exe',
		icon: 'oversoul',
		title: 'Artix Entertainment - OverSoul',
		swfURL: 'https://oversoul.artix.com/game/OSWrapper0_5_00.swf',
		baseURL: 'https://oversoul.artix.com/game/',
		width: '960',
		height: '580',
		graphics: 'vulkan'
	}
};

app.artixRuffleProcesses = app.artixRuffleProcesses || {};

app.resolveArtixRuffleExe = (gameInfo) => {
	const binDir = path.join(process.resourcesPath || path.join(__dirname, '..'), 'bin');
	return {
		exeName: gameInfo.exe,
		exePath: path.join(binDir, gameInfo.exe),
	};
};

app.launchArtixRuffleGame = (gameName) => {
	const gameInfo = app.artixRuffleGames[gameName];
	if (!gameInfo) {
		return false;
	}

	const resolvedExe = app.resolveArtixRuffleExe(gameInfo);
	if (app.artixRuffleProcesses[gameName]) {
		devToolsLog(resolvedExe.exeName + ' is already running for ' + gameName);
		return true;
	}

	const args = [
		gameInfo.swfURL,
		'--spoof-url', gameInfo.swfURL,
		'--base', gameInfo.baseURL,
		'--graphics', gameInfo.graphics || 'vulkan',
		'--quality', 'low',
		'--power', 'high',
		'--frame-rate', '24',
		'--width', gameInfo.width,
		'--height', gameInfo.height,
		'--scale', 'show-all',
		'--letterbox', 'on',
		'--upgrade-to-https',
		'--player-version', '32',
		'-m', '60',
		'--no-gui',
		'--tcp-connections', 'allow'
	];

	if (!fs.existsSync(resolvedExe.exePath)) {
		devToolsLog(gameInfo.exe + ' not found at ' + resolvedExe.exePath);
		return true;
	}

	try {
		const childEnv = Object.assign({}, process.env, {
			ARTIX_RUFFLE_WINDOW_TITLE: gameInfo.title,
			ARTIX_RUFFLE_GAME: gameName,
			ARTIX_RUFFLE_GAME_ICON: gameInfo.icon,
			RUST_LOG: 'warn',
			// SSAA 1.25x for AQW only (sharpens its fine vector lineart); '1' = off.
			RUFFLE_AQW_SUPERSAMPLE: gameName === 'aqw' ? '1.25' : '1'
		});

		const ruffleProcess = spawn(resolvedExe.exePath, args, {
			detached: true,
			stdio: 'ignore',
			windowsHide: true,
			env: childEnv
		});
		app.artixRuffleProcesses[gameName] = ruffleProcess;
		ruffleProcess.on('exit', () => {
			app.artixRuffleProcesses[gameName] = null;
		});
		ruffleProcess.on('error', () => {
			app.artixRuffleProcesses[gameName] = null;
		});
		ruffleProcess.unref();
		devToolsLog('spawned ' + resolvedExe.exeName + ' for ' + gameName);
	} catch (error) {
		app.artixRuffleProcesses[gameName] = null;
		devToolsLog('Error spawning ' + resolvedExe.exeName + ': ' + error.message);
	}

	return true;
};

app.launchAQWExe = () => {
	app.launchArtixRuffleGame('aqw');
};

`;

const artixRuffleLauncherRegex = /app\.artixRuffleGames = \{[\s\S]*?app\.launchAQWExe = \(\) => \{\r?\n\tapp\.launchArtixRuffleGame\('aqw'\);\r?\n\};\r?\n/;
if (artixRuffleLauncherRegex.test(main)) {
  main = main.replace(artixRuffleLauncherRegex, artixRuffleLauncher);
} else {
  if (!main.includes(insertBefore)) {
    throw new Error("Could not find launchGameLocal insertion point");
  }
  main = main.replace(insertBefore, artixRuffleLauncher + insertBefore);
}

main = main.replace(
  /'--scale', 'exact-fit',\r?\n\t\t'--force-scale',/,
  "'--scale', 'show-all',\r\n\t\t'--letterbox', 'on',"
);

if (!main.includes("app.injectAqTubeTab = () =>")) {
  if (!main.includes(insertBefore)) {
    throw new Error("Could not find AQTUBE insertion point");
  }
  main = main.replace(insertBefore, aqTubePatch + insertBefore);
}

if (!main.includes("app.injectAqTubeTab();")) {
  main = main.replace(
    "mainWindow = win;",
    "mainWindow = win;\r\n\r\n\tmainWindow.webContents.on('dom-ready', () => {\r\n\t\tapp.injectAqTubeTab();\r\n\t});"
  );
  main = main.replace(
    "mainWindow.show();\r\n\t});",
    "mainWindow.show();\r\n\t\tapp.injectAqTubeTab();\r\n\t});"
  );
}

if (!main.includes("app.launchArtixRuffleGame(gameName)")) {
  const localStartRegex = /app\.launchGameLocal = \(gameName\) => \{\r?\n\t(?:if \(gameName === 'aqw'\) \{\r?\n\t\tapp\.launchAQWExe\(\);\r?\n\t\treturn;\r?\n\t\}\r?\n\t)?if\(gameWindows\[gameName\] == undefined\)\{/;
  if (!localStartRegex.test(main)) {
    throw new Error("Could not find launchGameLocal body patch point");
  }
  main = main.replace(
    localStartRegex,
    "app.launchGameLocal = (gameName) => {\r\n\tif (app.launchArtixRuffleGame(gameName)) {\r\n\t\treturn;\r\n\t}\r\n\tif(gameWindows[gameName] == undefined){"
  );

  const ruffleStartRegex = /app\.launchRuffle = \(gameName\) => \{\r?\n\tif\(gameWindows\[gameName\] == undefined\)\{/;
  if (!ruffleStartRegex.test(main)) {
    throw new Error("Could not find launchRuffle body patch point");
  }
  main = main.replace(
    ruffleStartRegex,
    "app.launchRuffle = (gameName) => {\r\n\tif (app.launchArtixRuffleGame(gameName)) {\r\n\t\treturn;\r\n\t}\r\n\tif(gameWindows[gameName] == undefined){"
  );
}

const launcherWindowTitle = 'GameLauncher on Artix Entertainment v.230';
main = main.replaceAll("'Artix Game Launcher v.' + clientVersion", `'${launcherWindowTitle}'`);
main = main.replaceAll("title: 'Loading Artix Games',", "title: '" + launcherWindowTitle + "',");
main = main.replaceAll("mainWindow.once('page-title-updated', function (event) {", "mainWindow.on('page-title-updated', function (event) {");
main = main.replaceAll("mainWindow.title = '" + launcherWindowTitle + "';", "mainWindow.setTitle('" + launcherWindowTitle + "');");
main = main.replaceAll("appIcon.setToolTip('" + launcherWindowTitle + "');", "appIcon.setToolTip('" + launcherWindowTitle + "');");
if (!main.includes("mainWindow.setTitle('" + launcherWindowTitle + "');\r\n\t\tclearTimeout(mainWinTimer);")) {
  main = main.replace(
    "mainWindow.on('page-title-updated', function (event) {\r\n\t\tclearTimeout(mainWinTimer);",
    "mainWindow.on('page-title-updated', function (event) {\r\n\t\tevent.preventDefault();\r\n\t\tmainWindow.setTitle('" + launcherWindowTitle + "');\r\n\t\tclearTimeout(mainWinTimer);"
  );
}
if (!main.includes("mainWindow.show();\r\n\t\tmainWindow.setTitle('" + launcherWindowTitle + "');")) {
  main = main.replace(
    "mainWindow.show();\r\n\t\tapp.injectAqTubeTab();",
    "mainWindow.show();\r\n\t\tmainWindow.setTitle('" + launcherWindowTitle + "');\r\n\t\tapp.injectAqTubeTab();"
  );
}

contents.set("main.js", Buffer.from(main, "utf8"));

let offset = 0;
const dataBuffers = [];
for (const file of files) {
  const buffer = contents.get(file.path);
  file.entry.size = buffer.length;
  file.entry.offset = String(offset);
  dataBuffers.push(buffer);
  offset += buffer.length;
}

const headerJson = Buffer.from(JSON.stringify(header), "utf8");
const padding = (4 - (headerJson.length % 4)) % 4;
const payloadSize = 4 + headerJson.length + padding;
const headerBuffer = Buffer.alloc(4 + payloadSize);
headerBuffer.writeUInt32LE(payloadSize, 0);
headerBuffer.writeUInt32LE(headerJson.length, 4);
headerJson.copy(headerBuffer, 8);

const sizePickle = Buffer.alloc(8);
sizePickle.writeUInt32LE(4, 0);
sizePickle.writeUInt32LE(headerBuffer.length, 4);

fs.mkdirSync(path.dirname(outputAsar), { recursive: true });
fs.writeFileSync(outputAsar, Buffer.concat([sizePickle, headerBuffer, ...dataBuffers]));
'@

$nodeScriptPath = Join-Path $OutputDir "patch-artix-asar.js"
Set-Content -LiteralPath $nodeScriptPath -Value $nodeScript -Encoding UTF8
node $nodeScriptPath $sourceAsar $patchedAsar

$preload = Get-Content -LiteralPath $sourcePreload -Raw
if ($preload -notmatch "openAqTubeWindow") {
    $preload = $preload.TrimEnd() + @'

if (window.interop) {
    window.interop.openAqTubeWindow = function () {
        const { remote } = require('electron');
        remote.app.openAqTubeWindow();
    };
}
'@
}
Set-Content -LiteralPath $patchedPreload -Value $preload -Encoding UTF8

foreach ($entry in $gameExePaths.GetEnumerator()) {
    Copy-Item -LiteralPath $entry.Value -Destination (Join-Path $patchedBinDir $entry.Key) -Force
}

Write-Host "Patched ASAR written to $patchedAsar"
Write-Host "Patched preload written to $patchedPreload"
Write-Host "Ruffle executables written to $patchedBinDir"

if ($Install) {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    $isAdmin = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if (-not $isAdmin) {
        $args = @(
            "-NoProfile",
            "-ExecutionPolicy", "Bypass",
            "-File", "`"$PSCommandPath`"",
            "-ArtixLauncherPath", "`"$ArtixLauncherPath`"",
            "-AqwExePath", "`"$AqwExePath`"",
            "-OutputDir", "`"$OutputDir`"",
            "-Install"
        )
        Start-Process -FilePath "powershell.exe" -ArgumentList $args -Verb RunAs -Wait
        return
    }

    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $backupAsar = Join-Path $resourcesPath "app.asar.bak-aqw-test-$stamp"
    $backupPreload = Join-Path $resourcesPath "preload.js.bak-aqw-test-$stamp"
    $binDir = Join-Path $resourcesPath "bin"

    Copy-Item -LiteralPath $sourceAsar -Destination $backupAsar -Force
    Copy-Item -LiteralPath $sourcePreload -Destination $backupPreload -Force
    New-Item -ItemType Directory -Path $binDir -Force | Out-Null
    foreach ($entry in $gameExePaths.GetEnumerator()) {
        Copy-Item -LiteralPath $entry.Value -Destination (Join-Path $binDir $entry.Key) -Force
    }
    Copy-Item -LiteralPath $patchedAsar -Destination $sourceAsar -Force
    Copy-Item -LiteralPath $patchedPreload -Destination $sourcePreload -Force

    Write-Host "Installed patched ASAR."
    Write-Host "Backup: $backupAsar"
    Write-Host "Preload backup: $backupPreload"
    Write-Host "Ruffle executables: $binDir"
}
