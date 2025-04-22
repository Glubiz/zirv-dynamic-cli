<#
.SYNOPSIS
  Updates the Zirv Chocolatey package with a new version and Windows binary.

.PARAMETER Version
  The new version string (e.g. "0.6.4").

.PARAMETER ArtifactPath
  Relative path to the Windows executable produced by the CI 
  (e.g. "artifacts/zirv-0.6.4-windows.exe").

.EXAMPLE
  ./scripts/update_chocolatey.ps1 -Version 0.6.4 -ArtifactPath artifacts/zirv-0.6.4-windows.exe
#>

param(
    [Parameter(Mandatory = $true)]
    [string]$Version,

    [Parameter(Mandatory = $true)]
    [string]$ArtifactPath
)

# Determine paths
$repoRoot      = (Resolve-Path "$PSScriptRoot\..").Path
$packageFolder = Join-Path $repoRoot "chocolatey\zirv"
$toolsFolder   = Join-Path $packageFolder "tools"

Write-Host "Repository Root:      $repoRoot"
Write-Host "Package Folder:       $packageFolder"
Write-Host "Chocolatey Tools Dir: $toolsFolder"

# Ensure tools folder exists
if (-not (Test-Path $toolsFolder)) {
    Write-Host "Creating tools folder at '$toolsFolder'"
    New-Item -ItemType Directory -Path $toolsFolder | Out-Null
}

# Copy and rename Windows exe
$windowsSource      = Join-Path $repoRoot $ArtifactPath
$windowsDestination = Join-Path $toolsFolder "zirv.exe"

if (-not (Test-Path $windowsSource)) {
    Write-Error "Windows artifact not found at '$windowsSource'"
    exit 1
}

Write-Host "Copying Windows artifact..."
Copy-Item $windowsSource -Destination $windowsDestination -Force
Write-Host "  -> $windowsDestination"

# Update nuspec version
$nuspecPath = Join-Path $packageFolder "zirv.nuspec"
[xml]$nuspec = Get-Content $nuspecPath
$nuspec.package.metadata.version = $Version
$nuspec.Save($nuspecPath)
Write-Host "Updated nuspec version to $Version"

# Pack into the package folder
Write-Host "Packing the Chocolatey package into $packageFolder..."
choco pack $nuspecPath -o $packageFolder | Write-Host

# Expected package name
$pkgName    = "zirv.$Version.nupkg"
$expected   = Join-Path $packageFolder $pkgName

# If it's not where we told choco to put it, search fallback locations
if (-not (Test-Path $expected)) {
    Write-Host "Package not found at $expected"
    # 1) look anywhere under packageFolder
    $found = Get-ChildItem -Path $packageFolder -Filter $pkgName -Recurse -File -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $found) {
        # 2) look under repo root
        $found = Get-ChildItem -Path $repoRoot -Filter $pkgName -Recurse -File -ErrorAction SilentlyContinue | Select-Object -First 1
    }
    if ($found) {
        Write-Host "Found package at $($found.FullName)"
        $expected = $found.FullName
    } else {
        Write-Error "Could not locate '$pkgName' anywhere under $packageFolder or $repoRoot"
        exit 1
    }
}

Write-Host "Pushing package: $expected"
choco push $expected --api-key $env:CHOCOLATEY_API_KEY --source "https://push.chocolatey.org/"

Write-Host "✅ Chocolatey pushed successfully!"