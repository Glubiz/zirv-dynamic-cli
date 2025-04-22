<#
.SYNOPSIS
  Updates the Zirv Chocolatey package with a new version and Windows binary.

.PARAMETER Version
  The new version string (e.g. "0.6.2").

.PARAMETER ArtifactPath
  Relative path to the Windows executable produced by the CI 
  (e.g. "artifacts\zirv-0.6.2-windows.exe").

.EXAMPLE
  ./scripts/update_chocolatey.ps1 -Version 0.6.2 -ArtifactPath artifacts\zirv-0.6.2-windows.exe
#>

param(
    [Parameter(Mandatory = $true)]
    [string]$Version,

    [Parameter(Mandatory = $true)]
    [string]$ArtifactPath
)

# Determine the repository root (assuming this script lives in scripts\)
$repoRoot       = (Resolve-Path "$PSScriptRoot\..").Path
$packageFolder  = Join-Path $repoRoot "chocolatey\zirv"
$toolsFolder    = Join-Path $packageFolder "tools"

Write-Host "Repository Root:      $repoRoot"
Write-Host "Package Folder:       $packageFolder"
Write-Host "Chocolatey Tools Dir: $toolsFolder"

# Ensure the tools directory exists
if (-not (Test-Path $toolsFolder)) {
    Write-Host "Creating tools folder at '$toolsFolder'"
    New-Item -ItemType Directory -Path $toolsFolder | Out-Null
}

# Copy (and rename) the Windows artifact into tools\zirv.exe
$windowsArtifactSource      = Join-Path $repoRoot $ArtifactPath
$windowsArtifactDestination = Join-Path $toolsFolder "zirv.exe"

if (Test-Path $windowsArtifactSource) {
    Write-Host "Copying Windows artifact..."
    Copy-Item $windowsArtifactSource -Destination $windowsArtifactDestination -Force
    Write-Host "  Source:      $windowsArtifactSource"
    Write-Host "  Destination: $windowsArtifactDestination"
}
else {
    Write-Error "Windows artifact not found at '$windowsArtifactSource'"
    exit 1
}

# Path to the nuspec file
$nuspecPath = Join-Path $packageFolder "zirv.nuspec"

# Load and update the nuspec XML
[xml]$nuspec = Get-Content $nuspecPath

Write-Host "Updating nuspec version to $Version"
$nuspec.package.metadata.version = $Version
$nuspec.Save($nuspecPath)

# Pack the Chocolatey package
Write-Host "Packing the Chocolatey package..."
choco pack $nuspecPath -o $packageFolder | Write-Host

# The only change: include $Version in the expected package name
$expectedPackageName = "zirv.$Version.nupkg"
$packageFile        = Join-Path $packageFolder $expectedPackageName

if (-not (Test-Path $packageFile)) {
    Write-Host "Package not found at expected path: $packageFile"
    # Try finding it elsewhere
    $found = Get-ChildItem -Path $packageFolder -Filter "zirv.$Version.nupkg" -Recurse | Select-Object -First 1
    if ($found) {
        Write-Host "Found package at $($found.FullName)"
        $packageFile = $found.FullName
    }
    else {
        Write-Error "Package file 'zirv.$Version.nupkg' not found under $packageFolder"
        exit 1
    }
}

Write-Host "Pushing package: $packageFile"
choco push $packageFile `
    --api-key $env:CHOCOLATEY_API_KEY `
    --source "https://push.chocolatey.org/"

Write-Host "Done."
