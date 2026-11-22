param([string]$OutputDirectory = (Join-Path $PSScriptRoot '..\dist'))

$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Push-Location $root
$staging = $null
try {
    cargo build --release --locked -p agent-chat-reader
    if ($LASTEXITCODE -ne 0) { throw 'Release build failed.' }
    $output = [IO.Path]::GetFullPath($OutputDirectory)
    [void](New-Item -ItemType Directory -Force -Path $output)
    $staging = (New-Item -ItemType Directory -Path (Join-Path $output ('.package-' + [guid]::NewGuid()))).FullName
    $bundle = Join-Path $staging 'AgentChatReader'
    [void](New-Item -ItemType Directory -Force -Path $bundle)
    Copy-Item -LiteralPath 'target\release\AgentChatReader.exe' -Destination $bundle
    Copy-Item -LiteralPath 'README.md' -Destination $bundle
    $metadataJson = cargo metadata --format-version 1 --locked --filter-platform x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw 'Cannot inspect dependency licenses.' }
    $metadata = $metadataJson | ConvertFrom-Json
    $notices = [Collections.Generic.List[string]]::new()
    $notices.Add("AgentChatReader - third-party dependency notices`n")
    $licenses = Join-Path $bundle 'licenses'
    [void](New-Item -ItemType Directory -Force -Path $licenses)
    foreach ($package in ($metadata.packages | Sort-Object name, version)) {
        if ($package.name -in @('agent-chat-reader', 'agent-chat-reader-core')) { continue }
        $notices.Add("$($package.name) $($package.version)`nLicense: $($package.license)`n$($package.repository)`n")
        $directory = Split-Path -Parent $package.manifest_path
        $files = @(Get-ChildItem -LiteralPath $directory -File | Where-Object {
            $_.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE)([._-]|$)'
        })
        if ($package.license_file) {
            $files += Get-Item -LiteralPath (Join-Path $directory $package.license_file)
        }
        if ($files.Count -gt 0) {
            $destination = Join-Path $licenses "$($package.name)-$($package.version)"
            [void](New-Item -ItemType Directory -Force -Path $destination)
            foreach ($file in $files) { Copy-Item -LiteralPath $file.FullName -Destination $destination }
        }
    }
    $notices | Set-Content -LiteralPath (Join-Path $bundle 'THIRD-PARTY-NOTICES.txt') -Encoding utf8
    foreach ($file in (Get-ChildItem -LiteralPath $bundle -File -Recurse)) {
        if ($file.LastWriteTime.Year -lt 1980) { $file.LastWriteTime = [datetime]'1980-01-02' }
    }
    $archive = Join-Path $output 'AgentChatReader-windows-x64.zip'
    Compress-Archive -LiteralPath $bundle -DestinationPath $archive -Force
    $published = Join-Path $output 'AgentChatReader'
    [void](New-Item -ItemType Directory -Force -Path $published)
    foreach ($item in (Get-ChildItem -LiteralPath $bundle)) {
        Copy-Item -LiteralPath $item.FullName -Destination $published -Recurse -Force
    }
    Write-Output $archive
} finally {
    if ($staging) { Remove-Item -LiteralPath $staging -Recurse -Force }
    Pop-Location
}
