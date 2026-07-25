$ErrorActionPreference = "Stop"

$Repository = "algonacci/kumo"
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\kumo\bin"

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "Kumo currently requires 64-bit Windows."
}

$Target = "x86_64-pc-windows-msvc"
$Archive = "kumo-$Target.zip"
$ReleaseUrl = "https://github.com/$Repository/releases/latest/download"
$TempDir = Join-Path ([IO.Path]::GetTempPath()) "kumo-install-$([Guid]::NewGuid())"

try {
    New-Item -ItemType Directory -Force -Path $TempDir, $InstallDir | Out-Null
    $ArchivePath = Join-Path $TempDir $Archive
    $ChecksumPath = "$ArchivePath.sha256"

    Invoke-WebRequest "$ReleaseUrl/$Archive" -OutFile $ArchivePath
    Invoke-WebRequest "$ReleaseUrl/$Archive.sha256" -OutFile $ChecksumPath

    $Expected = (Get-Content $ChecksumPath -Raw).Split()[0].ToLower()
    $Actual = (Get-FileHash -Algorithm SHA256 $ArchivePath).Hash.ToLower()
    if ($Expected -ne $Actual) {
        throw "Checksum verification failed."
    }

    Expand-Archive -Path $ArchivePath -DestinationPath $TempDir -Force
    Copy-Item (Join-Path $TempDir "kumo.exe") (Join-Path $InstallDir "kumo.exe") -Force

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $PathEntries = @($UserPath -split ";" | Where-Object { $_ })
    if ($InstallDir -notin $PathEntries) {
        $NewPath = (($PathEntries + $InstallDir) -join ";")
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        $env:Path = "$env:Path;$InstallDir"
        Write-Host "Added $InstallDir to your user PATH."
    }

    Write-Host "Kumo installed to $InstallDir\kumo.exe"
    Write-Host ""
    Write-Host "Next steps (open a new terminal first):"
    Write-Host "  kumo                 Run onboarding and start Kumo in the foreground"
    Write-Host "  kumo start           Run Kumo detached in the background (after onboarding)"
    Write-Host "  kumo status          Check on a running instance"
    Write-Host "  kumo doctor          Check configuration and connectivity"
    Write-Host ""
    Write-Host "Note: 'kumo enable' (start automatically on login) is not yet supported on" -ForegroundColor Yellow
    Write-Host "Windows; use 'kumo start' each session, or Task Scheduler yourself." -ForegroundColor Yellow
}
finally {
    Remove-Item -Recurse -Force $TempDir -ErrorAction SilentlyContinue
}
