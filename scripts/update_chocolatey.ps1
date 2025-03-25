param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$ArtifactPath
)

# Define the package folder relative to the repository root
$packageFolder = "chocolatey\zirv"

# Change directory into the package folder so the nuspec is in context
Push-Location $packageFolder

# Update the nuspec file using XML (ensuring valid XML structure)
[xml]$nuspec = Get-Content "zirv.nuspec"
$nuspec.package.metadata.version = $Version
$nuspec.Save("zirv.nuspec")

Pop-Location

# Pack the Chocolatey package; force output to the package folder
choco pack "$packageFolder\zirv.nuspec" -o "$packageFolder"

# Define the expected path for the generated .nupkg file
$packagePath = Join-Path $packageFolder "zirv.$Version.nupkg"

if (-not (Test-Path $packagePath)) {
    Write-Error "File not found: '$packagePath'."
    exit 1
}

# Push the package to Chocolatey (ensure the push source is set)
choco push $packagePath --api-key $env:CHOCOLATEY_API_KEY --source "https://push.chocolatey.org/"
