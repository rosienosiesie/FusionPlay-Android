param(
    [string]$ToolsRoot = $env:FUSIONPLAY_TOOLS_ROOT,
    [string]$FlutterSdk,
    [string]$AndroidSdk
)

$ErrorActionPreference = 'Stop'

$projectPath = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$repositoryRoot = [System.IO.Path]::GetFullPath((Join-Path $projectPath '..'))

if (-not [string]::IsNullOrWhiteSpace($ToolsRoot)) {
    $ToolsRoot = [System.IO.Path]::GetFullPath($ToolsRoot)
    if ([string]::IsNullOrWhiteSpace($FlutterSdk)) {
        $FlutterSdk = Join-Path $ToolsRoot 'flutter\3.32.8'
    }
    if ([string]::IsNullOrWhiteSpace($AndroidSdk)) {
        $AndroidSdk = Join-Path $ToolsRoot 'android-sdk'
    }

    $cacheRoot = Join-Path $ToolsRoot 'cache'
    $env:TEMP = Join-Path $cacheRoot 'temp\FusionPlay'
    $env:TMP = $env:TEMP
    $env:PUB_CACHE = Join-Path $cacheRoot 'pub'
    $env:GRADLE_USER_HOME = Join-Path $cacheRoot 'gradle'
    $env:CARGO_TARGET_DIR = Join-Path $cacheRoot 'cargo-target\FusionPlay'
    if ([string]::IsNullOrWhiteSpace($env:CARGO_HOME)) {
        $env:CARGO_HOME = Join-Path $ToolsRoot 'rust\cargo-home'
    }
    if ([string]::IsNullOrWhiteSpace($env:RUSTUP_HOME)) {
        $env:RUSTUP_HOME = Join-Path $ToolsRoot 'rust\rustup-home'
    }

    foreach ($directory in @(
        $env:TEMP,
        $env:PUB_CACHE,
        $env:GRADLE_USER_HOME,
        $env:CARGO_TARGET_DIR,
        $env:CARGO_HOME,
        $env:RUSTUP_HOME
    )) {
        [System.IO.Directory]::CreateDirectory($directory) | Out-Null
    }
}

if ([string]::IsNullOrWhiteSpace($FlutterSdk)) {
    $FlutterSdk = $env:FLUTTER_ROOT
}
if ([string]::IsNullOrWhiteSpace($FlutterSdk)) {
    $flutterCommand = Get-Command flutter.bat, flutter -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $flutterCommand) {
        throw 'Flutter was not found. Set FLUTTER_ROOT or add Flutter to PATH.'
    }
    $FlutterSdk = Split-Path (Split-Path $flutterCommand.Source -Parent) -Parent
}

if ([string]::IsNullOrWhiteSpace($AndroidSdk)) {
    $AndroidSdk = $env:ANDROID_HOME
}
if ([string]::IsNullOrWhiteSpace($AndroidSdk)) {
    $AndroidSdk = $env:ANDROID_SDK_ROOT
}
if ([string]::IsNullOrWhiteSpace($AndroidSdk)) {
    throw 'Android SDK was not found. Set ANDROID_HOME or ANDROID_SDK_ROOT.'
}

$FlutterSdk = [System.IO.Path]::GetFullPath($FlutterSdk)
$AndroidSdk = [System.IO.Path]::GetFullPath($AndroidSdk)
if (-not (Test-Path -LiteralPath (Join-Path $FlutterSdk 'bin\flutter.bat'))) {
    throw "Flutter SDK is incomplete: $FlutterSdk"
}
if (-not (Test-Path -LiteralPath (Join-Path $AndroidSdk 'platform-tools\adb.exe'))) {
    throw "Android SDK is incomplete: $AndroidSdk"
}
if ([string]::IsNullOrWhiteSpace($env:CARGO_HOME)) {
    $userProfile = [Environment]::GetFolderPath('UserProfile')
    $env:CARGO_HOME = Join-Path $userProfile '.cargo'
}

$fusionPlayVersion = (Get-Content -LiteralPath (Join-Path $repositoryRoot 'VERSION') -Raw).Trim()
if ($fusionPlayVersion -notmatch '^(\d+)\.(\d+)\.(\d+)(?:[ -].*)?$') {
    throw "VERSION must begin with major.minor.patch: '$fusionPlayVersion'"
}
$fusionPlayVersionCode =
    [int]$Matches[1] * 10000 + [int]$Matches[2] * 100 + [int]$Matches[3]

$mappedDrives = [System.Collections.Generic.List[string]]::new()
function Add-DriveMapping([string]$Path) {
    foreach ($letter in @('Z', 'Y', 'X', 'W', 'V', 'U')) {
        $drive = "$letter`:"
        if (-not (Test-Path -LiteralPath "$drive\")) {
            & subst.exe $drive $Path
            if ($LASTEXITCODE -ne 0) {
                throw "Unable to map $Path to $drive"
            }
            $script:mappedDrives.Add($drive)
            return "$drive\"
        }
    }
    throw 'No free drive letter is available for an ASCII build path.'
}

$localPropertiesPath = Join-Path $projectPath 'android\local.properties'
$hadLocalProperties = Test-Path -LiteralPath $localPropertiesPath
$originalLocalProperties = if ($hadLocalProperties) {
    [System.IO.File]::ReadAllBytes($localPropertiesPath)
} else {
    $null
}

try {
    $mappedRepositoryRoot = Add-DriveMapping $repositoryRoot
    $mappedFlutterRoot = Add-DriveMapping $FlutterSdk
    $mappedAndroidRoot = Add-DriveMapping $AndroidSdk
    $mappedProjectRoot = Join-Path $mappedRepositoryRoot 'flutter'

    $env:FLUTTER_ROOT = $mappedFlutterRoot
    $env:ANDROID_HOME = $mappedAndroidRoot
    $env:ANDROID_SDK_ROOT = $mappedAndroidRoot

    $localProperties = if ($hadLocalProperties) {
        [System.Collections.Generic.List[string]](
            [System.IO.File]::ReadAllLines($localPropertiesPath)
        )
    } else {
        [System.Collections.Generic.List[string]]::new()
    }
    foreach ($property in @(
        "sdk.dir=$($mappedAndroidRoot.Replace('\', '/'))",
        "flutter.sdk=$($mappedFlutterRoot.Replace('\', '/'))"
    )) {
        $name = $property.Substring(0, $property.IndexOf('=') + 1)
        $index = -1
        for ($lineIndex = 0; $lineIndex -lt $localProperties.Count; $lineIndex++) {
            if ($localProperties[$lineIndex].StartsWith($name)) {
                $index = $lineIndex
                break
            }
        }
        if ($index -ge 0) {
            $localProperties[$index] = $property
        } else {
            $localProperties.Add($property)
        }
    }
    [System.IO.File]::WriteAllLines(
        $localPropertiesPath,
        $localProperties,
        [System.Text.UTF8Encoding]::new($false)
    )

    $flutter = Join-Path $mappedFlutterRoot 'bin\flutter.bat'
    Push-Location $mappedProjectRoot
    try {
        & $flutter pub get --enforce-lockfile
        if ($LASTEXITCODE -ne 0) { throw 'flutter pub get failed.' }
        $symbolsPath = Join-Path $mappedProjectRoot 'build\symbols'
        & $flutter build apk --release --no-pub `
            --build-name=$fusionPlayVersion `
            --build-number=$fusionPlayVersionCode `
            --target-platform android-arm,android-arm64,android-x64 `
            --obfuscate `
            "--dart-define=FUSIONPLAY_VERSION=$fusionPlayVersion" `
            --split-debug-info=$symbolsPath
        if ($LASTEXITCODE -ne 0) { throw 'Flutter universal APK build failed.' }

        $releaseDirectory = Join-Path $repositoryRoot 'artifacts\releases'
        [System.IO.Directory]::CreateDirectory($releaseDirectory) | Out-Null
        $apkSource = Join-Path $mappedProjectRoot 'build\app\outputs\flutter-apk\app-release.apk'
        $apkDestination = Join-Path $releaseDirectory "FusionPlay-Android-$fusionPlayVersion.apk"
        Copy-Item -LiteralPath $apkSource -Destination $apkDestination -Force

        $symbolArchive = Join-Path $releaseDirectory "FusionPlay-$fusionPlayVersion-symbols.zip"
        Compress-Archive -Path (Join-Path $symbolsPath '*') `
            -DestinationPath $symbolArchive -CompressionLevel Optimal -Force
        Write-Host "APK: $apkDestination"
        Write-Host "Symbols: $symbolArchive"
    } finally {
        Pop-Location
    }
} finally {
    if ($hadLocalProperties) {
        [System.IO.File]::WriteAllBytes($localPropertiesPath, $originalLocalProperties)
    } else {
        [System.IO.File]::Delete($localPropertiesPath)
    }
    for ($index = $mappedDrives.Count - 1; $index -ge 0; $index--) {
        & subst.exe $mappedDrives[$index] /d | Out-Null
    }
}
