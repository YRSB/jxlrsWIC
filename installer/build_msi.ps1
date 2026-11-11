param (
    [Parameter(Mandatory = $true)]
    [string]$Target,

    [Parameter(Mandatory = $true)]
    [string]$DllPath,

    [Parameter(Mandatory = $false)]
    [string]$OutputDir = "dist"
)

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path "$PSScriptRoot\..").Path
$resolvedDll = (Resolve-Path $DllPath).Path

if (-not (Test-Path $resolvedDll)) {
    throw "Target DLL not found: $DllPath"
}

# Ensure .dotnet tools is on PATH
$dotnetTools = Join-Path $env:USERPROFILE ".dotnet\tools"
if ((Test-Path $dotnetTools) -and ($env:PATH -notlike "*$dotnetTools*")) {
    $env:PATH = "$dotnetTools;$env:PATH"
}

# Ensure WiX v4 tool is installed
$needInstall = $true
if (Get-Command wix -ErrorAction SilentlyContinue) {
    $currentVer = (& wix --version 2>&1) -join " "
    if ($currentVer -like "4.*") {
        $needInstall = $false
    } else {
        Write-Host "Current WiX ($currentVer) is not v4. Uninstalling..."
        dotnet tool uninstall --global wix
    }
}

if ($needInstall) {
    Write-Host "Installing WiX v4.0.6 via dotnet tool..."
    dotnet tool install --global wix --version 4.0.6
    if ((Test-Path $dotnetTools) -and ($env:PATH -notlike "*$dotnetTools*")) {
        $env:PATH = "$dotnetTools;$env:PATH"
    }
}

$wixVersion = & wix --version
Write-Host "Using WiX Toolset: $wixVersion"

# Map Rust target triple to WiX architecture
$arch = switch ($Target) {
    "x86_64-pc-windows-msvc" { "x64" }
    "aarch64-pc-windows-msvc" { "arm64" }
    "i686-pc-windows-msvc"    { "x86" }
    default { throw "Unsupported target triple: $Target" }
}

# Extract 4-part version from Cargo.toml (e.g., 0.1.4 -> 0.1.4.0)
$cargoToml = Get-Content "$repoRoot\Cargo.toml" -Raw
if ($cargoToml -match 'version\s*=\s*"([^"]+)"') {
    $crateVersion = $matches[1]
} else {
    throw "Unable to extract version from Cargo.toml"
}

$parts = [System.Collections.Generic.List[string]]($crateVersion.Split('.'))
while ($parts.Count -lt 4) {
    $parts.Add("0")
}
$msiVersion = $parts -join "."

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$outputMsi = Join-Path (Resolve-Path $OutputDir).Path "jxlrs_wic-$Target.msi"
$wxsFile = Join-Path $repoRoot "installer\jxlrs_wic.wxs"

Write-Host "Building MSI installer for $Target (arch: $arch, version: $msiVersion)..."
Write-Host "Source DLL: $resolvedDll"
Write-Host "Output MSI: $outputMsi"

& wix build `
    -arch $arch `
    -d Version="$msiVersion" `
    -d SourceDll="$resolvedDll" `
    -d SourceDir="$repoRoot" `
    -o "$outputMsi" `
    "$wxsFile"

if ($LASTEXITCODE -ne 0) {
    throw "WiX build failed with exit code $LASTEXITCODE"
}

$sha256 = (Get-FileHash $outputMsi -Algorithm SHA256).Hash.ToLowerInvariant()
Set-Content -Path "$outputMsi.sha256" -Value $sha256 -NoNewline
Write-Host "Successfully generated: $outputMsi" -ForegroundColor Green
Write-Host "SHA-256: $sha256" -ForegroundColor Green
