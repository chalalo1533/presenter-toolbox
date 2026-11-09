<#
  Builds the Microsoft Store package (.msix) of Presenter Toolbox.

  Use the three values from Partner Center → Product management → Product identity:
    .\packaging\msix\build-msix.ps1 `
        -Name "12345Chalalo.PresenterToolbox" `
        -Publisher "CN=XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX" `
        -PublisherDisplayName "chalalo"

  -Test  builds with placeholder identity and installs it locally (needs Developer Mode).
  The Store signs the package itself, so the .msix for upload is left unsigned.
#>
param(
    [string]$Name = "PresenterToolbox.Dev",
    [string]$Publisher = "CN=PresenterToolboxDev",
    [string]$PublisherDisplayName = "chalalo",
    [switch]$Test,
    [switch]$SkipBuild
)
$ErrorActionPreference = "Stop"
$app = Resolve-Path (Join-Path $PSScriptRoot "..\..")
Set-Location $app

# Windows SDK tools (newest installed version)
$kits = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
$sdk = Get-ChildItem $kits -Directory | Where-Object { Test-Path (Join-Path $_.FullName "x64\makeappx.exe") } |
    Sort-Object { [version]$_.Name } | Select-Object -Last 1
if (-not $sdk) { throw "No encuentro makeappx.exe: instala el Windows SDK." }
$makeappx = Join-Path $sdk.FullName "x64\makeappx.exe"
$makepri = Join-Path $sdk.FullName "x64\makepri.exe"

# version: Cargo 0.1.0 -> MSIX 0.1.0.0 (the Store requires the 4th number to be 0)
$cargoVer = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
$version = "$cargoVer.0"

if (-not $SkipBuild) {
    $env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build falló" }
}

$out = Join-Path $app "target\msix"
$layout = Join-Path $out "layout"
Remove-Item $out -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force (Join-Path $layout "Assets") | Out-Null

Copy-Item "target\release\presenter_toolbox.exe" $layout
& "$layout\presenter_toolbox.exe" --write-msix-assets (Join-Path $layout "Assets")
Start-Sleep -Milliseconds 500

$manifest = Get-Content (Join-Path $PSScriptRoot "AppxManifest.template.xml") -Raw -Encoding UTF8
$manifest = $manifest.Replace("{{NAME}}", $Name).Replace("{{PUBLISHER}}", $Publisher).
    Replace("{{PUBLISHER_DISPLAY_NAME}}", $PublisherDisplayName).Replace("{{VERSION}}", $version)
[IO.File]::WriteAllText((Join-Path $layout "AppxManifest.xml"), $manifest, (New-Object Text.UTF8Encoding $false))

# resources.pri lets Windows pick the right icon size (scale-200, taskbar 'unplated', ...)
& $makepri createconfig /cf "$out\priconfig.xml" /dq es /pv 10.0.0 /o | Out-Null
# single package (no resource packs): keep every icon size inside one resources.pri
$cfg = [xml](Get-Content "$out\priconfig.xml" -Raw)
$pk = $cfg.SelectSingleNode("//packaging")
if ($pk) { [void]$pk.ParentNode.RemoveChild($pk) }
$cfg.Save("$out\priconfig.xml")
& $makepri new /pr $layout /cf "$out\priconfig.xml" /mn "$layout\AppxManifest.xml" /of "$layout\resources.pri" /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makepri falló" }

if ($Test) {
    Get-AppxPackage -Name $Name | Remove-AppxPackage -ErrorAction SilentlyContinue
    Add-AppxPackage -Register "$layout\AppxManifest.xml"
    "Instalado para prueba: búscalo en Inicio como 'Presenter Toolbox'."
    return
}

$dist = Join-Path $app "..\dist\store"
New-Item -ItemType Directory -Force $dist | Out-Null
$msix = Join-Path (Resolve-Path $dist) "PresenterToolbox_${version}_x64.msix"
& $makeappx pack /d $layout /p $msix /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makeappx falló" }
"Paquete listo para subir a Partner Center: $msix"
