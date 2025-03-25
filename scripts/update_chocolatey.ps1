param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$ArtifactPath
)

# Determine repository root (assuming this script is in the 'scripts' folder)
$repoRoot = (Resolve-Path "$PSScriptRoot\..").Path
# Define the package folder relative to the repository root (where your nuspec file is located)
$packageFolder = Join-Path $repoRoot "chocolatey\zirv"

Write-Host "Repository Root: $repoRoot"
Write-Host "Package Folder: $packageFolder"

# Define path for the nuspec file
$nuspecPath = Join-Path $packageFolder "zirv.nuspec"

# Update the nuspec file using XML to ensure valid XML structure
[xml]$nuspec = Get-Content $nuspecPath
$nuspec.package.metadata.version = $Version
$nuspec.Save($nuspecPath)

# Ensure the Windows executable is present in the package folder.
# The nuspec file references "zirv-windows-latest.exe", so it must be present here.
$winExePath = Join-Path $packageFolder "zirv-windows-latest.exe"
if (-not (Test-Path $winExePath)) {
    Write-Host "Copying Windows artifact from '$ArtifactPath' to '$winExePath'"
    if (Test-Path $ArtifactPath) {
         Copy-Item $ArtifactPath -Destination $winExePath
    }
    else {
         Write-Error "Artifact not found at provided path: $ArtifactPath"
         exit 1
    }
}

# Pack the Chocolatey package with output forced to $packageFolder
choco pack $nuspecPath -o $packageFolder

# Define the expected path for the generated .nupkg file
$packageFile = Join-Path $packageFolder "zirv.$Version.nupkg"

if (-not (Test-Path $packageFile)) {
    Write-Error "File not found: '$packageFile'."
    exit 1
}

Write-Host "Package file located at: $packageFile"

# Push the package to Chocolatey
choco push $packageFile --api-key $env:CHOCOLATEY_API_KEY --source "https://push.chocolatey.org/"
