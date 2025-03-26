param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$ArtifactPath
)

# Determine the repository root (assuming this script is in the 'scripts' folder)
$repoRoot = (Resolve-Path "$PSScriptRoot\..").Path
# Define the package folder relative to the repository root (where your nuspec file resides)
$packageFolder = Join-Path $repoRoot "chocolatey\zirv"

Write-Host "Repository Root: $repoRoot"
Write-Host "Package Folder: $packageFolder"

# Copy the Windows artifact into the package folder so it can be included in the package.
$windowsArtifactSource = Join-Path $repoRoot $ArtifactPath
$windowsArtifactDestination = Join-Path $packageFolder "zirv-windows-latest.exe"
if (Test-Path $windowsArtifactSource) {
    Write-Host "Copying Windows artifact from '$windowsArtifactSource' to '$windowsArtifactDestination'"
    Copy-Item $windowsArtifactSource -Destination $windowsArtifactDestination -Force
} else {
    Write-Host "Windows artifact not found at '$windowsArtifactSource'"
    # Optionally, you could exit if this artifact is required.
    # exit 1
}

# Path to the nuspec file
$nuspecPath = Join-Path $packageFolder "zirv.nuspec"

# Update the nuspec file using XML to ensure valid XML structure
[xml]$nuspec = Get-Content $nuspecPath
$nuspec.package.metadata.version = $Version
$nuspec.Save($nuspecPath)
Write-Host "Updated nuspec version to $Version"

# Pack the Chocolatey package with output forced to $packageFolder
choco pack $nuspecPath -o $packageFolder

# Define the expected package file name
$expectedFile = "zirv.$Version.nupkg"
$packageFile = Join-Path $packageFolder $expectedFile

if (-not (Test-Path $packageFile)) {
    Write-Host "Package file not found in package folder: $packageFile"
    
    # Search in repository root
    $rootFile = Join-Path $repoRoot $expectedFile
    if (Test-Path $rootFile) {
        Write-Host "Found package in repository root: $rootFile. Moving to package folder..."
        Move-Item $rootFile -Destination $packageFolder
        $packageFile = Join-Path $packageFolder $expectedFile
    } else {
        Write-Host "Package file not found at expected path in repository root: $rootFile"
        Write-Host "Searching repository recursively for $expectedFile..."
        $pkg = Get-ChildItem -Path $repoRoot -Filter $expectedFile -Recurse | Select-Object -First 1
        if ($pkg) {
            Write-Host "Found package at $($pkg.FullName). Moving to package folder..."
            Move-Item $pkg.FullName -Destination $packageFolder
            $packageFile = Join-Path $packageFolder $expectedFile
        }
    }
    
    if (-not (Test-Path $packageFile)) {
        Write-Error "File not found: '$packageFile'."
        exit 1
    }
}

Write-Host "Package file located at: $packageFile"

# Push the package to Chocolatey
choco push $packageFile --api-key $env:CHOCOLATEY_API_KEY --source "https://push.chocolatey.org/"
