param (
    [Parameter(Mandatory = $true)]
    [string]$DllPath,

    [switch]$Unregistered
)

$ErrorActionPreference = "Stop"

$CLSID = "{466C040F-E9A9-45F4-BE8A-BF0386B15E28}"
$CATID = "{7ED96837-96F0-4812-B211-F13C24117ED3}"
$CONTAINER_FORMAT = "{FEC14E3F-427A-4736-AAE6-27ED84F69322}"

if ($Unregistered) {
    Write-Host "Verifying unregistration cleanup for CLSID $CLSID..."
    $clsidPath = "Registry::HKEY_CLASSES_ROOT\CLSID\$CLSID"
    $catidPath = "Registry::HKEY_CLASSES_ROOT\CLSID\$CATID\Instance\$CLSID"

    if (Test-Path $clsidPath) {
        throw "Failed: CLSID key $clsidPath still exists after unregistration!"
    }
    if (Test-Path $catidPath) {
        throw "Failed: CATID Instance key $catidPath still exists after unregistration!"
    }
    Write-Host "Unregistration verified: all keys successfully removed." -ForegroundColor Green
    exit 0
}

Write-Host "Verifying WIC COM registration for: $DllPath" -ForegroundColor Cyan

# 1. Check registry keys
$clsidPath = "Registry::HKEY_CLASSES_ROOT\CLSID\$CLSID"
if (-not (Test-Path $clsidPath)) {
    throw "Verification failed: Registry key $clsidPath does not exist!"
}

$friendlyName = (Get-ItemProperty -Path $clsidPath -Name "FriendlyName").FriendlyName
if ($friendlyName -ne "jxlrsWIC JPEG XL Decoder") {
    throw "Verification failed: FriendlyName expected 'jxlrsWIC JPEG XL Decoder', got '$friendlyName'"
}

$containerFormat = (Get-ItemProperty -Path $clsidPath -Name "ContainerFormat").ContainerFormat
if ($containerFormat -ne $CONTAINER_FORMAT) {
    throw "Verification failed: ContainerFormat expected '$CONTAINER_FORMAT', got '$containerFormat'"
}

$inprocPath = "$clsidPath\InprocServer32"
if (-not (Test-Path $inprocPath)) {
    throw "Verification failed: $inprocPath does not exist!"
}
$registeredDll = (Get-ItemProperty -Path $inprocPath).'(default)'
$resolvedDll = (Resolve-Path $DllPath).Path
if ($registeredDll -ne $resolvedDll) {
    throw "Verification failed: Registered DLL path mismatch. Expected '$resolvedDll', got '$registeredDll'"
}

$threadingModel = (Get-ItemProperty -Path $inprocPath -Name "ThreadingModel").ThreadingModel
if ($threadingModel -ne "Both") {
    throw "Verification failed: ThreadingModel expected 'Both', got '$threadingModel'"
}

# 2. Check Patterns
$pattern0 = "$clsidPath\Patterns\0"
$pattern1 = "$clsidPath\Patterns\1"
if (-not (Test-Path $pattern0) -or -not (Test-Path $pattern1)) {
    throw "Verification failed: Sniffing patterns not registered properly under $clsidPath\Patterns"
}

# 3. Check CATID_WICBitmapDecoders instance
$catidPath = "Registry::HKEY_CLASSES_ROOT\CLSID\$CATID\Instance\$CLSID"
if (-not (Test-Path $catidPath)) {
    throw "Verification failed: Decoder category registration $catidPath does not exist!"
}

Write-Host "Registry verification passed successfully." -ForegroundColor Green

# 4. COM Instantiation check via .NET Activator
Write-Host "Instantiating COM server via CoCreateInstance (Type::GetTypeFromCLSID)..."
$type = [Type]::GetTypeFromCLSID([Guid]$CLSID)
if ($null -eq $type) {
    throw "Verification failed: [Type]::GetTypeFromCLSID returned null for $CLSID"
}

$instance = [Activator]::CreateInstance($type)
if ($null -eq $instance) {
    throw "Verification failed: [Activator]::CreateInstance failed to create COM instance"
}

Write-Host "COM activation succeeded: Object created ($instance)" -ForegroundColor Green
[System.Runtime.InteropServices.Marshal]::ReleaseComObject($instance) | Out-Null
Write-Host "All WIC COM registration and activation checks passed!" -ForegroundColor Green
exit 0
