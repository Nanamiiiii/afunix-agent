[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+$')]
    [string] $PackageVersion,

    [Parameter(Mandatory)]
    [string] $InstallerUrl,

    [Parameter(Mandatory)]
    [ValidatePattern('^[A-Fa-f0-9]{64}$')]
    [string] $InstallerSha256,

    [Parameter(Mandatory)]
    [string] $OutputDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$uri = $InstallerUrl -as [Uri]
if ($null -eq $uri -or !$uri.IsAbsoluteUri -or $uri.Scheme -ne 'https') {
    throw 'InstallerUrl must be an absolute HTTPS URL.'
}

$packageIdentifier = 'Nanamiiiii.afunix-agent'
$manifestVersion = '1.10.0'
$outputPath = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Force $outputPath | Out-Null

$versionManifest = @"
# yaml-language-server: `$schema=https://aka.ms/winget-manifest.version.$manifestVersion.schema.json

PackageIdentifier: $packageIdentifier
PackageVersion: $PackageVersion
DefaultLocale: en-US
ManifestType: version
ManifestVersion: $manifestVersion
"@

$installerManifest = @"
# yaml-language-server: `$schema=https://aka.ms/winget-manifest.installer.$manifestVersion.schema.json

PackageIdentifier: $packageIdentifier
PackageVersion: $PackageVersion
InstallerType: portable
Scope: user
MinimumOSVersion: 10.0.17134.0
UpgradeBehavior: install
Commands:
- afunix-agent
Installers:
- Architecture: x64
  InstallerUrl: $InstallerUrl
  InstallerSha256: $($InstallerSha256.ToUpperInvariant())
ManifestType: installer
ManifestVersion: $manifestVersion
"@

$localeManifest = @"
# yaml-language-server: `$schema=https://aka.ms/winget-manifest.defaultLocale.$manifestVersion.schema.json

PackageIdentifier: $packageIdentifier
PackageVersion: $PackageVersion
PackageLocale: en-US
Publisher: Nanamiiiii
PublisherUrl: https://github.com/Nanamiiiii
PublisherSupportUrl: https://github.com/Nanamiiiii/afunix-agent/issues
PackageName: afunix-agent
PackageUrl: https://github.com/Nanamiiiii/afunix-agent
License: MIT
LicenseUrl: https://github.com/Nanamiiiii/afunix-agent/blob/v$PackageVersion/LICENSE
ShortDescription: Expose a Windows SSH agent backend through a native AF_UNIX socket
Description: Exposes the Pageant-compatible interface provided by GnuPG gpg-agent through a native Windows AF_UNIX socket.
Moniker: afunix-agent
Tags:
- agent
- gnupg
- pageant
- ssh
- unix-socket
ReleaseNotesUrl: https://github.com/Nanamiiiii/afunix-agent/releases/tag/v$PackageVersion
ManifestType: defaultLocale
ManifestVersion: $manifestVersion
"@

$encoding = 'utf8NoBOM'
Set-Content -LiteralPath (Join-Path $outputPath "$packageIdentifier.yaml") -Value $versionManifest -Encoding $encoding
Set-Content -LiteralPath (Join-Path $outputPath "$packageIdentifier.installer.yaml") -Value $installerManifest -Encoding $encoding
Set-Content -LiteralPath (Join-Path $outputPath "$packageIdentifier.locale.en-US.yaml") -Value $localeManifest -Encoding $encoding

Get-ChildItem -LiteralPath $outputPath -File | Sort-Object Name | ForEach-Object FullName
