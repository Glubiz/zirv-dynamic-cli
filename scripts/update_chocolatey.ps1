param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$ArtifactPath
)

# Determine the absolute path to the repository root (assuming this script is in the 'scripts' folder)
$repoRoot = (Resolve-Path "$PSScriptRoot\..").Path
# Define the package folder (where your nuspec file resides)
$packageFolder = Join-Path $repoRoot "chocolatey\zirv"

Write-Host "Repository Root: $repoRoot"
Write-Host "Package Folder: $packageFolder"

# Update the nuspec file using XML to ensure valid XML
$nuspecPath = Join-Path $packageFolder "zirv.nuspec"
[xml]$nuspec = Get-Content $nuspecPath
$nuspec.package.metadata.version = $Version
$nuspec.Save($nuspecPath)

# Pack the Chocolatey package, forcing output to the package folder using an absolute path
choco pack $nuspecPath -o $packageFolder

# Define the expected package file path
$packageFile = Join-Path $packageFolder "zirv.$Version.nupkg"

if (-not (Test-Path $packageFile)) {
    Write-Error "File not found: '$packageFile'."
    exit 1
}

# Push the package to Chocolatey
choco push $packageFile --api-key $env:CHOCOLATEY_API_KEY --source "https://push.chocolatey.org/"
