$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ("neuro-sync-installer-test-" + [Guid]::NewGuid().ToString("N"))
$assets = Join-Path $testRoot "assets"
$stage = Join-Path $testRoot "stage"
$rendered = Join-Path $testRoot "rendered"
$version = "9.8.7"
$packageStem = "neuro-sync-v$version-windows-x86_64-UNSIGNED-PILOT"
$package = "$packageStem.zip"

try {
  $packageRoot = Join-Path $stage $packageStem
  New-Item -ItemType Directory -Force -Path (Join-Path $packageRoot "libexec"), $assets | Out-Null
  Set-Content -Encoding ascii (Join-Path $packageRoot "neuro-sync.exe") "test client"
  Set-Content -Encoding ascii (Join-Path $packageRoot "libexec\dcm2niix.exe") "test converter"
  Add-Type -AssemblyName System.IO.Compression.FileSystem
  $archive = Join-Path $assets $package
  [IO.Compression.ZipFile]::CreateFromDirectory($stage, $archive)
  $sha = (Get-FileHash -Algorithm SHA256 $archive).Hash.ToLowerInvariant()
  $unused = "0" * 64
  $downloadBase = ([Uri]$assets).AbsoluteUri.TrimEnd("/")

  & bash (Join-Path $root "scripts/render-installers.sh") `
    $rendered $version $downloadBase `
    "unused-macos.zip" $unused `
    "unused-linux.tar.gz" $unused `
    $package $sha
  if ($LASTEXITCODE -ne 0) { throw "installer rendering failed" }
  $renderedInstaller = Get-Content -Raw (Join-Path $rendered "install.ps1")
  if ($renderedInstaller.Contains("Start-Process")) {
    throw "installer must not start another process"
  }
  if ($renderedInstaller.Contains("NEURO_SYNC_NO_LAUNCH") -or $renderedInstaller.Contains("Starting terminal setup") -or $renderedInstaller.Contains('& (Join-Path $binDir "neuro-sync.exe")')) {
    throw "installer still launches neuro-sync automatically"
  }
  if (-not $renderedInstaller.Contains("Installation complete. Find or copy your DICOM folder path, then run:")) {
    throw "installer is missing the explicit next-step message"
  }

  $env:NEURO_SYNC_INSTALL_ROOT = Join-Path $testRoot "install"
  $env:NEURO_SYNC_BIN_DIR = Join-Path $env:NEURO_SYNC_INSTALL_ROOT "bin"
  $env:NEURO_SYNC_NO_PATH_UPDATE = "1"
  & (Join-Path $rendered "install.ps1")

  if (-not (Test-Path -PathType Leaf (Join-Path $env:NEURO_SYNC_BIN_DIR "neuro-sync.exe"))) {
    throw "neuro-sync.exe was not installed"
  }
  if (-not (Test-Path -PathType Leaf (Join-Path $env:NEURO_SYNC_BIN_DIR "libexec\dcm2niix.exe"))) {
    throw "dcm2niix.exe was not installed"
  }

  Add-Content -Encoding ascii $archive "tampered"
  $env:NEURO_SYNC_INSTALL_ROOT = Join-Path $testRoot "bad-install"
  $env:NEURO_SYNC_BIN_DIR = Join-Path $env:NEURO_SYNC_INSTALL_ROOT "bin"
  $failed = $false
  try {
    & (Join-Path $rendered "install.ps1")
  } catch {
    $failed = $true
  }
  if (-not $failed) { throw "tampered package was accepted" }
  if (Test-Path $env:NEURO_SYNC_BIN_DIR) { throw "tampered package installed files" }

  Write-Host "terminal installer smoke test passed on Windows"
} finally {
  Remove-Item Env:NEURO_SYNC_INSTALL_ROOT -ErrorAction SilentlyContinue
  Remove-Item Env:NEURO_SYNC_BIN_DIR -ErrorAction SilentlyContinue
  Remove-Item Env:NEURO_SYNC_NO_PATH_UPDATE -ErrorAction SilentlyContinue
  if (Test-Path $testRoot) { Remove-Item -Recurse -Force $testRoot }
}
