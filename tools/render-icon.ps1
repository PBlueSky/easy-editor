# 重新生成应用图标：assets/easy-editor.svg -> assets/build/*.png(+*.bgra) -> assets/easy-editor.ico
#
#   powershell -ExecutionPolicy Bypass -File tools/render-icon.ps1
#
# 光栅化用本机 Chrome/Edge 的无头截图，缩小和拼 .ico 交给 node（tools/make-ico.mjs）。
# 只改了 SVG 的话跑这一个脚本就够；中间产物 assets/build/ 不进版本库，
# assets/easy-editor.ico 和 assets/easy-editor-256.png 是要提交的。
#
# 本文件必须存成「UTF-8 带 BOM」：Windows PowerShell 5.1 否则按 GBK 读，中文注释能把解析搞崩。
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$root  = Split-Path -Parent $PSScriptRoot
$svg   = Join-Path $root 'assets\easy-editor.svg'
$build = Join-Path $root 'assets\build'

# 只渲两张：1024 母图给 node 缩小成 16..128，256 直接当 PNG 塞进 .ico。
# 不逐档找 Chrome 要图——它在 window-size=128/160 会稳定截出全透明的空图。
$masterSize = 1024
$pngSize    = 256

if (-not (Test-Path $svg)) { throw "找不到图标源文件：$svg" }
New-Item -ItemType Directory -Force -Path $build | Out-Null

function Find-Browser {
    $cands = @(
        "$env:ProgramFiles\Google\Chrome\Application\chrome.exe",
        "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe",
        "$env:ProgramFiles\Microsoft\Edge\Application\msedge.exe",
        "${env:ProgramFiles(x86)}\Microsoft\Edge\Application\msedge.exe"
    )
    foreach ($c in $cands) { if (Test-Path $c) { return (Resolve-Path $c).Path } }
    throw "没找到 Chrome 或 Edge，无法把 SVG 光栅化成 PNG。"
}

Add-Type -AssemblyName System.Drawing

# 隔 8 个像素抽一个点看 alpha，别用 GetPixel 遍历整张 1024（那是上百万次调用）。
function Test-Painted($path, $size) {
    $bmp = [System.Drawing.Bitmap]::FromFile($path)
    try {
        if ($bmp.Width -ne $size -or $bmp.Height -ne $size) {
            throw "$path 尺寸不对：$($bmp.Width)x$($bmp.Height)，应为 ${size}x${size}"
        }
        $hit = 0
        for ($y = 0; $y -lt $size; $y += 8) {
            for ($x = 0; $x -lt $size; $x += 8) {
                if ($bmp.GetPixel($x, $y).A -gt 0) { $hit++ }
            }
        }
        return $hit
    }
    finally { $bmp.Dispose() }
}

function Render-Png($browser, $profile, $html, $size, $png) {
    if (Test-Path $png) { Remove-Item $png -Force }
    $chromeArgs = @(
        '--headless=new', '--disable-gpu', '--hide-scrollbars',
        '--default-background-color=00000000',
        '--force-device-scale-factor=1',
        "--user-data-dir=$profile",
        "--window-size=$size,$size",
        '--virtual-time-budget=3000',
        "--screenshot=$png",
        $html
    )
    # Chrome 把「xx bytes written to file」打在 stderr 上，$ErrorActionPreference=Stop
    # 会把这行正常输出当成终止性错误，所以这一句单独放宽。
    $eap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    & $browser @chromeArgs 2>&1 | Out-Null
    $ErrorActionPreference = $eap
}

$browser = Find-Browser
Write-Host "光栅化器：$browser"

# 无头截图截的是整个窗口，所以拿一个铺满窗口的 <img> 包住 SVG。
# SVG 的 width/height 属性会被 CSS 覆盖，缩放交给浏览器。
$html = Join-Path $build 'render.html'
$src  = ($svg -replace '\\', '/') -replace ' ', '%20'
@"
<!doctype html><meta charset="utf-8">
<style>html,body{margin:0;padding:0}img{position:fixed;inset:0;width:100%;height:100%}</style>
<img src="file:///$src">
"@ | Set-Content -Path $html -Encoding ASCII
$htmlPath = (Get-Item $html).FullName

$profile = Join-Path $env:TEMP ("easy-editor-icon-" + [guid]::NewGuid().ToString('N'))
try {
    foreach ($size in @($masterSize, $pngSize)) {
        $png = Join-Path $build "$size.png"
        $hit = 0
        for ($try = 1; $try -le 3; $try++) {
            Render-Png $browser $profile $htmlPath $size $png
            if (-not (Test-Path $png)) { Write-Warning "$size.png 第 $try 次没写出文件，重试"; continue }
            $hit = Test-Painted $png $size
            if ($hit -gt 0) { break }
            Write-Warning "$size.png 第 $try 次是全透明的，重画"
        }
        if ($hit -le 0) { throw "$size.png 渲染不出东西（抽样命中 $hit 个不透明像素）。" }
        Write-Host ("  {0,5}px -> {1,8} bytes, 抽样不透明点 {2}" -f $size, (Get-Item $png).Length, $hit)
    }
}
finally {
    if (Test-Path $profile) { Remove-Item $profile -Recurse -Force -ErrorAction SilentlyContinue }
}

# GDI+ 把母图解成 32bpp 的 BGRA 裸数据，node 直接读这些字节缩小，
# 省得在 node 里再实现一遍 PNG 解码。
$bmp = [System.Drawing.Bitmap]::FromFile((Join-Path $build "$masterSize.png"))
try {
    $rect = New-Object System.Drawing.Rectangle 0, 0, $masterSize, $masterSize
    $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly,
                          [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    try {
        $bytes = New-Object byte[] ($masterSize * $masterSize * 4)
        [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $bytes, 0, $bytes.Length)
        [System.IO.File]::WriteAllBytes((Join-Path $build "$masterSize.bgra"), $bytes)
    }
    finally { $bmp.UnlockBits($data) }
}
finally { $bmp.Dispose() }

Copy-Item (Join-Path $build "$pngSize.png") (Join-Path $root 'assets\easy-editor-256.png') -Force

& node (Join-Path $root 'tools\make-ico.mjs') $build (Join-Path $root 'assets\easy-editor.ico')
if ($LASTEXITCODE -ne 0) { throw "make-ico.mjs 失败，退出码 $LASTEXITCODE" }

Write-Host "完成：assets/easy-editor.ico + assets/easy-editor-256.png"
