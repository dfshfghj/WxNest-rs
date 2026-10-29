param(
    [string]$OutDir = (Join-Path (Resolve-Path (Join-Path $PSScriptRoot '..')) 'target\bridge'),
    [string]$CopyTo = ''
)

$ErrorActionPreference = 'Stop'

foreach ($tool in @('cl.exe', 'lib.exe', 'dumpbin.exe')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "missing $tool; run from an MSVC x64 developer environment (vcvars64.bat)."
    }
}
if ($env:VSCMD_ARG_TGT_ARCH -and $env:VSCMD_ARG_TGT_ARCH -ne 'x64') {
    throw 'MSVC x64 target is required.'
}

$repo = Resolve-Path (Join-Path $PSScriptRoot '..')
$native = Join-Path $repo 'vendor\helper-native\native'
$obj = Join-Path $OutDir 'obj'
New-Item -ItemType Directory -Force -Path $obj | Out-Null
$dll = Join-Path $OutDir 'text_bridge.dll'

Push-Location $obj
try {
    $rel = (Resolve-Path -Relative $native).TrimEnd('\')
    $minhook = "$rel\vendor\minhook-1.3.4"
    & cl.exe @(
        '/nologo', '/c', '/TC', '/O2', '/MD', '/DNDEBUG', '/W3', '/utf-8',
        "/I$minhook\include", "/I$minhook\src", "/I$minhook\src\hde",
        "$minhook\src\buffer.c", "$minhook\src\hook.c", "$minhook\src\trampoline.c",
        "$minhook\src\hde\hde64.c"
    )
    if ($LASTEXITCODE -ne 0) { throw 'MinHook compile failed.' }
    & lib.exe /nologo /out:minhook.lib buffer.obj hook.obj trampoline.obj hde64.obj
    if ($LASTEXITCODE -ne 0) { throw 'MinHook lib failed.' }
    & cl.exe @(
        '/nologo', '/std:c++17', '/EHsc', '/W4', '/utf-8', '/LD', '/MD', '/O2',
        '/DNDEBUG', '/Brepro', "$rel\text_bridge.cpp",
        "/Fe:$dll", '/Fo:text_bridge.obj',
        '/link', '/MACHINE:X64', '/Brepro', 'bcrypt.lib', 'minhook.lib'
    )
    if ($LASTEXITCODE -ne 0) { throw 'text_bridge.dll compile failed.' }
} finally {
    Pop-Location
}

$exports = (& dumpbin.exe /nologo /exports $dll | Out-String)
foreach ($name in @(
    'WxSendText', 'WxSendImage', 'WxSendFile', 'WxSendEmoticon', 'WxSendVoice',
    'WxSendAtText', 'WxPublishMoments', 'WxMomentsInteractionStart',
    'WxMomentsInteractionPoll', 'WxMomentsInteractionClose', 'WxInspectCoreVersion'
)) {
    if ($exports -notmatch "\b$name\b") {
        throw "text_bridge.dll missing export $name"
    }
}

if ($CopyTo) {
    $parent = Split-Path -Parent $CopyTo
    if ($parent) { New-Item -ItemType Directory -Force -Path $parent | Out-Null }
    Copy-Item -LiteralPath $dll -Destination $CopyTo -Force
    Write-Host "copied: $CopyTo"
}

$item = Get-Item -LiteralPath $dll
Write-Host ("built: {0} ({1} bytes)" -f $item.FullName, $item.Length)
