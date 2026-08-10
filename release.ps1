# Build a Kestrel release: version-stamped images plus checksums.
#
# Output lands in release\kestrel-<version>\, ready to attach to a GitHub
# release. The version comes from kernel/Cargo.toml so there is one place to
# bump it, and it is the same string the kernel prints in its banner and in
# `version`.
#
# Usage:  .\release.ps1

$ErrorActionPreference = "Stop"

$root = $PSScriptRoot
Push-Location $root
try {
    $manifest = Get-Content "$root\kernel\Cargo.toml" -Raw
    if ($manifest -notmatch '(?m)^version\s*=\s*"([^"]+)"') {
        throw "could not read the version from kernel/Cargo.toml"
    }
    $version = $Matches[1]
    Write-Host "Building Kestrel v$version"

    # The website carries the version in its badge, which nothing else checks.
    # A stale badge is the sort of thing nobody notices until someone downloads
    # the wrong thing, so refuse to build a release that disagrees with it.
    #
    # Keep this file ASCII-only. Windows PowerShell 5.1 reads a UTF-8 script
    # with no BOM as CP1252, where an em dash ends in 0x94 - a curly quote -
    # which unbalances the next string and reports itself as a missing brace
    # two dozen lines away.
    $site = "$root\docs\index.html"
    if (Test-Path $site) {
        $badge = Get-Content $site -Raw
        if ($badge -notmatch [regex]::Escape("v$version beta")) {
            throw "docs/index.html does not say 'v$version beta'; update the badge before releasing"
        }
    }

    # The console draws from an 8x8 ASCII font, so any other character reaches
    # the screen as one question mark per UTF-8 byte. An em dash in a message
    # nobody happened to trigger shipped in 0.9.1 that way. Comments never
    # reach the screen, so only the rest is checked.
    $offenders = @()
    foreach ($file in Get-ChildItem "$root\kernel\src", "$root\user" -Recurse -Filter *.rs) {
        $number = 0
        foreach ($line in Get-Content $file.FullName -Encoding UTF8) {
            $number++
            if ($line.TrimStart().StartsWith("//")) { continue }
            if ($line -match "[^\x00-\x7F]") {
                $offenders += "  $($file.FullName):$number`: $($line.Trim())"
            }
        }
    }
    if ($offenders.Count -gt 0) {
        throw "non-ASCII in code the console can print:`n" + ($offenders -join "`n")
    }

    # Always release-profile: the debug kernel is ~18x larger and takes about
    # half a minute to boot off an emulated CD.
    cargo xtask image --release
    if ($LASTEXITCODE -ne 0) { throw "image build failed" }

    $out = "$root\release\kestrel-$version"
    New-Item -ItemType Directory -Force -Path $out | Out-Null

    $artifacts = @(
        @{ From = "build\kestrel.iso"; To = "kestrel-$version.iso" },
        @{ From = "build\kestrel.vhd"; To = "kestrel-$version.vhd" },
        @{ From = "build\kestrel.img"; To = "kestrel-$version.img" }
    )

    foreach ($artifact in $artifacts) {
        $source = Join-Path $root $artifact.From
        if (-not (Test-Path $source)) { throw "missing build output: $($artifact.From)" }
        Copy-Item $source (Join-Path $out $artifact.To) -Force
    }

    Copy-Item "$root\README.md" "$out\README.md" -Force
    Copy-Item "$root\LICENSE" "$out\LICENSE" -Force

    # Checksums let someone verify a download; without them a truncated image
    # looks exactly like a kernel that fails to boot.
    $lines = foreach ($artifact in $artifacts) {
        $file = Join-Path $out $artifact.To
        $hash = (Get-FileHash $file -Algorithm SHA256).Hash.ToLower()
        "$hash  $($artifact.To)"
    }
    $lines | Set-Content "$out\SHA256SUMS" -Encoding ascii

    Write-Host ""
    Write-Host "Release staged in release\kestrel-$version\"
    foreach ($artifact in $artifacts) {
        $file = Get-Item (Join-Path $out $artifact.To)
        "{0,-28} {1,8:N1} MiB" -f $file.Name, ($file.Length / 1MB) | Write-Host
    }
    Write-Host ""
    Get-Content "$out\SHA256SUMS" | Write-Host
} finally {
    Pop-Location
}
