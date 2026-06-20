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

$resourcesPath = Join-Path $ArtixLauncherPath "resources"
$sourceAsar = Join-Path $resourcesPath "app.asar"
$patchedAsar = Join-Path $OutputDir "app.asar"

if (-not (Get-Command node -ErrorAction SilentlyContinue)) {
    throw "Node.js is required to patch app.asar."
}
if (-not (Test-Path -LiteralPath $sourceAsar)) {
    throw "Could not find Artix launcher app.asar at $sourceAsar"
}
if (-not (Test-Path -LiteralPath $AqwExePath)) {
    throw "Could not find AQW.exe at $AqwExePath"
}

New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null

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

const insertBefore = "app.launchGameLocal = (gameName) => {";
const aqwLauncher = String.raw`
app.launchAQWExe = () => {
	const gameInfo = gameWindows.aqw;
	const aqwExePath = path.join(process.resourcesPath || path.join(__dirname, '..'), 'bin', 'AQW.exe');
	const swfURL = gameInfo.url;
	const baseURL = 'https://game.aq.com/game/gamefiles/';
	const args = [
		swfURL,
		'--spoof-url', swfURL,
		'--base', baseURL,
		'--graphics', 'vulkan',
		'--quality', 'low',
		'--power', 'high',
		'--frame-rate', '24',
		'--scale', 'exact-fit',
		'--force-scale',
		'--upgrade-to-https',
		'--player-version', '32',
		'-m', '60',
		'--no-gui',
		'--tcp-connections', 'allow'
	];

	if (!fs.existsSync(aqwExePath)) {
		devToolsLog('AQW.exe not found at ' + aqwExePath);
		return;
	}

	try {
		const aqwProcess = spawn(aqwExePath, args, {
			detached: true,
			stdio: 'ignore',
			windowsHide: true,
			env: Object.assign({}, process.env, {
				ARTIX_RUFFLE_WINDOW_TITLE: 'Artix Entertainment - AdventureQuest Worlds V0.2',
				RUST_LOG: 'warn'
			})
		});
		aqwProcess.unref();
		devToolsLog('spawned AQW.exe for aqw');
	} catch (error) {
		devToolsLog('Error spawning AQW.exe: ' + error.message);
	}
};

`;

if (!main.includes("app.launchAQWExe = () =>")) {
  if (!main.includes(insertBefore)) {
    throw new Error("Could not find launchGameLocal insertion point");
  }
  main = main.replace(insertBefore, aqwLauncher + insertBefore);
}

if (!main.includes("if (gameName === 'aqw')")) {
  const localStartRegex = /app\.launchGameLocal = \(gameName\) => \{\r?\n\tif\(gameWindows\[gameName\] == undefined\)\{/;
  if (!localStartRegex.test(main)) {
    throw new Error("Could not find launchGameLocal body patch point");
  }
  main = main.replace(
    localStartRegex,
    "app.launchGameLocal = (gameName) => {\r\n\tif (gameName === 'aqw') {\r\n\t\tapp.launchAQWExe();\r\n\t\treturn;\r\n\t}\r\n\tif(gameWindows[gameName] == undefined){"
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

Write-Host "Patched ASAR written to $patchedAsar"

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
    $binDir = Join-Path $resourcesPath "bin"

    Copy-Item -LiteralPath $sourceAsar -Destination $backupAsar -Force
    New-Item -ItemType Directory -Path $binDir -Force | Out-Null
    Copy-Item -LiteralPath $AqwExePath -Destination (Join-Path $binDir "AQW.exe") -Force
    Copy-Item -LiteralPath $patchedAsar -Destination $sourceAsar -Force

    Write-Host "Installed patched ASAR."
    Write-Host "Backup: $backupAsar"
    Write-Host "AQW.exe: $(Join-Path $binDir 'AQW.exe')"
}
