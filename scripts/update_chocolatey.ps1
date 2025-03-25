param(
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$ArtifactPath
)

# Update the nuspec file with the new version
(Get-Content "chocolatey/zirv/zirv.nuspec") -replace '(<version>)[^<]+(</version>)', "`$1$Version`$2" | Set-Content "chocolatey/zirv/zirv.nuspec"

# Pack the package
choco pack "chocolatey/zirv/zirv.nuspec" -o "chocolatey/zirv/"

# Push the package (if desired)
choco push "chocolatey/zirv/zirv.$Version.nupkg" --api-key $env:CHOCOLATEY_API_KEY
