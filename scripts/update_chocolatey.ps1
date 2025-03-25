param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$ArtifactPath
)

# Determine the repository root (assuming this script is in the 'scripts' folder)
$repoRoot = (Resolve-Path "$PSScriptRoot\..").Path
# Define the package folder relative to the repository root (where your nuspec is located)
$packageFolder = Join-Path $repoRoot "chocolatey\zirv"

Write-Host "Repository Root: $repoRoot"
Write-Host "Package Folder: $packageFolder"

# Path to the nuspec file
$nuspecPath = Join-Path $packageFolder "zirv.nuspec"

# Update the nuspec file using XML to ensure valid XML structure
[xml]$nuspec = Get-Content $nuspecPath
$nuspec.package.metadata.version = $Version
$nuspec.Save($nuspecPath)

# Pack the Chocolatey package with output forced to $packageFolder
choco pack $nuspecPath -o $packageFolder

# Define the expected path for the generated .nupkg file
$packageFile = Join-Path $packageFolder "zirv.$Version.nupkg"

# If the file is not found in the package folder, search the repository recursively
if (-not (Test-Path $packageFile)) {
    Write-Host "Package not found in $packageFolder. Searching repository root..."
    $pkg = Get-ChildItem -Path $repoRoot -Filter "zirv.$Version.nupkg" -Recurse | Select-Object -First 1
    if ($pkg) {
        Write-Host "Found package at $($pkg.FullName). Moving to $packageFolder..."
        Move-Item $pkg.FullName -Destination $packageFolder
        $packageFile = Join-Path $packageFolder "zirv.$Version.nupkg"
    }
    else {
        Write-Error "File not found: 'zirv.$Version.nupkg' in repository."
        exit 1
    }
}

Write-Host "Package file located at: $packageFile"

# Push the package to Chocolatey
choco push $packageFile --api-key $env:CHOCOLATEY_API_KEY --source "https://push.chocolatey.org/"
