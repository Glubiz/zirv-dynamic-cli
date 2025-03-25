param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$ArtifactPath
)

# Define the package folder relative to the repository root.
$packageFolder = "chocolatey\zirv"

# Change directory into the package folder so that the nuspec file is in context.
Push-Location $packageFolder

# Update the nuspec file using XML to ensure valid XML is maintained.
[xml]$nuspec = Get-Content "zirv.nuspec"
$nuspec.package.metadata.version = $Version
$nuspec.Save("zirv.nuspec")

# Pack the Chocolatey package. This should create a file named "zirv.$Version.nupkg" in the current folder.
choco pack "zirv.nuspec" -o .

# Define the expected package file path.
$packageFile = "zirv.$Version.nupkg"

if (-not (Test-Path $packageFile)) {
    Write-Error "File not found: '$packageFile'."
    Pop-Location
    exit 1
}

# Push the package to Chocolatey.
choco push $packageFile --api-key $env:CHOCOLATEY_API_KEY --source "https://push.chocolatey.org/"

Pop-Location
