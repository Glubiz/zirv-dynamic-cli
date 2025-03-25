param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$ArtifactPath
)

# Load the nuspec file as XML
[xml]$nuspec = Get-Content "chocolatey/zirv/zirv.nuspec"

# Update the version node
$nuspec.package.metadata.version = $Version

# Optionally update other fields (e.g., URL, checksum, etc.)

# Save the updated nuspec file
$nuspec.Save("chocolatey/zirv/zirv.nuspec")

# Now pack the package
choco pack "chocolatey/zirv/zirv.nuspec" -o "chocolatey/zirv/"

# Push the package (ensure your .nupkg file name matches)
choco push "chocolatey/zirv/zirv.$Version.nupkg" --api-key $env:CHOCOLATEY_API_KEY --source "https://push.chocolatey.org/"
