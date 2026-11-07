# ============================================================================
# HelloGate 品牌图标生成脚本 · AI Logo 版（可重复执行，随时再生成）
#
# 【这个脚本干什么】
# 把定稿的 AI 生成 Logo（assets\logo_master.jpg，原名 logo_candidate_flat.jpg，
# 首次运行自动改名）转成两样正式资产，覆盖旧的程序几何绘制版：
#   1. assets\hellogate.ico —— 多尺寸 ICO（16/24/32/48/64/128/256），编进 exe；
#   2. assets\hellogate.png —— 512×512 带透明圆角的 PNG，README 头图用。
# build.rs 已声明 rerun-if-changed=assets/hellogate.ico，覆盖后
# cargo build --release 会自动重新嵌入，无需改任何 Rust 代码。
#
# 【处理流程】
#   ① 裁白边：源图四周是白色留白（实测背景 253~254），扫描"非白像素"
#      （R/G/B 任一 < 220）的最小包围盒，Crop 出圆角方块本体。
#      ——为什么是 220 而不是 245：实测 245 会被 JPEG 压缩噪声（个别背景
#      像素 B 通道低到 236）和右下角淡投影污染，包围盒会一路膨胀到图像
#      边缘（1920×1920 全图）；而方块本体最浅处 R≈129，220 能干净地
#      把"本体"与"背景/噪声/投影"分开，实测包围盒对 180~220 完全稳定。
#   ② 缩放：对每个目标尺寸用"逐级对半缩 + 最后一刀 HighQualityBicubic"
#      缩放到 size×size。1586→16 这种近百倍的直接缩放容易走样，
#      逐级折半是位图缩采样最稳的经典做法。
#   ③ 圆角透明蒙版：底板之外的四个角要真正透明（ICO/PNG 都带 alpha 通道）。
#      逐像素计算圆角矩形的"符号距离场"（SDF），边缘 1px smoothstep 羽化
#      ——这是四种实现里最稳的：GDI+ 的 Region/SetClip 裁剪不做抗锯齿，
#      小尺寸下圆角会有明显锯齿；逐像素 SDF 天然平滑、结果可精确校验。
#      蒙版半径 = 实测源图圆角半径（见 ④），保证"与图中圆角一致"，
#      不裁掉蓝色本体、也不残留白边。
#   ④ 圆角半径自动实测：任务书预估 0.22×边长，但实测本图源角半径
#      ≈0.132×边长——若写死 0.22 会把四角蓝色"啃"掉一圈。因此脚本对
#      左上角弧线逐行采样"首个内容像素"，最小二乘拟合出真实半径；
#      拟合失败时回退 0.22。想手工钉死比例可改下面的 $CORNER_FRAC。
#   ⑤ 拼 ICO：ICONDIR(6B) + 7×ICONDIRENTRY(16B) + 依次追加各尺寸 PNG 数据。
#      Vista+ 的 ICO 容器允许 PNG 压缩条目，目标平台 Win10/11 完全支持。
#   ⑥ 自验：重新解析生成的 ico，打印条目数/各尺寸/偏移，并校验每个条目
#      偏移处确实是 PNG 签名 89504E470D0A1A0A。
#
# 【运行方式】在项目根目录执行：powershell -File tools\make_icon_ai.ps1
# 【编码要求】本文件必须保存为 UTF-8 带 BOM——Windows PowerShell 5.1
#   读无 BOM 文件时按 GBK 解析中文注释，会直接语法报错（本项目已知坑）。
# ============================================================================

Add-Type -AssemblyName System.Drawing

# ---------------------------------------------------------------------------
# 可调参数
# ---------------------------------------------------------------------------
$WHITE_THR   = 220   # "非白像素"判定阈值：R/G/B 任一低于它即算内容（见头注①）
$CORNER_FRAC = 0     # 圆角半径/边长；0 = 自动实测源图圆角（推荐），>0 = 强制钉死
$FEATHER_PX  = 1.0   # 蒙版边缘羽化宽度（像素），smoothstep 平滑过渡
$SIZES       = @(16, 24, 32, 48, 64, 128, 256)   # ICO 条目尺寸（升序）

# ---------------------------------------------------------------------------
# 路径准备；首次运行时把源图规范改名为 logo_master.jpg
# ---------------------------------------------------------------------------
$root   = Split-Path -Parent $PSScriptRoot          # 项目根目录（tools 的上一级）
$assets = Join-Path $root "assets"
if (-not (Test-Path $assets)) { New-Item -ItemType Directory -Path $assets | Out-Null }

$srcPath   = Join-Path $assets "logo_master.jpg"
$legacyJpg = Join-Path $assets "logo_candidate_flat.jpg"
if (-not (Test-Path $srcPath)) {
    if (Test-Path $legacyJpg) {
        # 定稿源图是一次性资产，改名进版本库长期引用；脚本幂等，再跑不会重复改名
        Move-Item -LiteralPath $legacyJpg -Destination $srcPath
        Write-Host ("源图已改名：logo_candidate_flat.jpg -> logo_master.jpg")
    } else {
        throw ("找不到 Logo 源图：{0}（也不要再叫 {1}），请确认 assets 目录" -f $srcPath, $legacyJpg)
    }
}

# ---------------------------------------------------------------------------
# 把 JPG 整幅锁进内存字节数组（24bpp，BGR 排列）。
# GetPixel 逐点取色在 1920×1920（约 370 万点）上慢到不可用，
# LockBits + Marshal.Copy 一次拷出来再按 stride 索引，快几个数量级。
# ---------------------------------------------------------------------------
$srcBmp = New-Object System.Drawing.Bitmap $srcPath
$srcW = $srcBmp.Width; $srcH = $srcBmp.Height
$fullRect = New-Object System.Drawing.Rectangle(0, 0, $srcW, $srcH)
$fmt24 = [System.Drawing.Imaging.PixelFormat]::Format24bppRgb
$bd = $srcBmp.LockBits($fullRect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, $fmt24)
$stride24 = [Math]::Abs($bd.Stride)
$px = New-Object byte[] ($stride24 * $srcH)
[System.Runtime.InteropServices.Marshal]::Copy($bd.Scan0, $px, 0, $px.Length)
$srcBmp.UnlockBits($bd)
Write-Host ("读入源图 {0}（{1}x{2}）" -f (Split-Path $srcPath -Leaf), $srcW, $srcH)

# 判断某像素是否"内容"（非白）：BGR 三通道任一 < $WHITE_THR
function Test-ContentPx([int]$x, [int]$y) {
    $i = $y * $stride24 + $x * 3
    return ($px[$i + 2] -lt $WHITE_THR) -or ($px[$i + 1] -lt $WHITE_THR) -or ($px[$i] -lt $WHITE_THR)
}

# ---------------------------------------------------------------------------
# ① 扫描非白像素最小包围盒（逐行/逐列统计内容像素数，取首尾非零行列）
# ---------------------------------------------------------------------------
$rowCount = New-Object int[] $srcH
$colCount = New-Object int[] $srcW
for ($y = 0; $y -lt $srcH; $y++) {
    $base = $y * $stride24
    $c = 0
    for ($x = 0; $x -lt $srcW; $x++) {
        $i = $base + $x * 3
        if ($px[$i + 2] -lt $WHITE_THR -or $px[$i + 1] -lt $WHITE_THR -or $px[$i] -lt $WHITE_THR) {
            $c++
            $colCount[$x]++
        }
    }
    $rowCount[$y] = $c
}
$bbX0 = -1; $bbY0 = -1; $bbX1 = -1; $bbY1 = -1
for ($y = 0; $y -lt $srcH; $y++) { if ($rowCount[$y] -gt 0) { if ($bbY0 -lt 0) { $bbY0 = $y }; $bbY1 = $y } }
for ($x = 0; $x -lt $srcW; $x++) { if ($colCount[$x] -gt 0) { if ($bbX0 -lt 0) { $bbX0 = $x }; $bbX1 = $x } }
if ($bbX0 -lt 0) { throw "整幅图都是白色，没找到 Logo 本体，检查源图/阈值" }
$bbW = $bbX1 - $bbX0 + 1
$bbH = $bbY1 - $bbY0 + 1
Write-Host ("裁掉白边：包围盒 x0={0} y0={1} x1={2} y1={3}，本体 {4}x{5}" -f $bbX0, $bbY0, $bbX1, $bbY1, $bbW, $bbH)

# ---------------------------------------------------------------------------
# ④ 自动实测源图圆角半径：
# 对左上角弧线逐行采样"该行第一个内容像素的 x 偏移"，
# 圆角理论曲线为 offset(dy) = R - sqrt(2·R·dy - dy²)（dy>=R 时为 0），
# 在候选 R 里做最小二乘取 SSE 最小者。
# ---------------------------------------------------------------------------
function Measure-CornerRadius {
    $samples = New-Object System.Collections.ArrayList
    $scanMax = [int]($bbW * 0.5)
    for ($dy = 2; $dy -le [int]($bbW * 0.35); $dy += 4) {
        $fx = -1
        $base = ($bbY0 + $dy) * $stride24
        for ($x = $bbX0; $x -lt $bbX0 + $scanMax; $x++) {
            $i = $base + $x * 3
            if ($px[$i + 2] -lt $WHITE_THR -or $px[$i + 1] -lt $WHITE_THR -or $px[$i] -lt $WHITE_THR) { $fx = $x; break }
        }
        if ($fx -lt 0) { continue }
        [void]$samples.Add(@{ dy = $dy; off = ($fx - $bbX0) })
    }
    if ($samples.Count -lt 8) { return -1.0 }
    $bestR = -1.0; $bestSse = [double]::MaxValue
    $rHi = [int]($bbW * 0.45)
    for ($R = [int]($bbW * 0.02); $R -le $rHi; $R++) {
        $sse = 0.0
        foreach ($s in $samples) {
            $dy = [double]$s.dy
            if ($dy -ge $R) { $pred = 0.0 }
            else { $pred = $R - [Math]::Sqrt([Math]::Max(2.0 * $R * $dy - $dy * $dy, 0.0)) }
            $e = $pred - $s.off
            $sse += $e * $e
        }
        if ($sse -lt $bestSse) { $bestSse = $sse; $bestR = $R }
    }
    return $bestR
}

$cornerR = Measure-CornerRadius
if ($CORNER_FRAC -gt 0) {
    $cornerFrac = $CORNER_FRAC
    Write-Host ("圆角比例：按参数强制 {0:N4}（半径 {1:N1}px）" -f $cornerFrac, ($cornerFrac * $bbW))
} elseif ($cornerR -gt 0) {
    $cornerFrac = $cornerR / $bbW
    Write-Host ("圆角比例：实测源图半径 {0}px / 边长 {1}px = {2:N4}（任务书预估 0.22，以实测为准，避免啃掉蓝角）" -f $cornerR, $bbW, $cornerFrac)
} else {
    $cornerFrac = 0.22
    Write-Host ("圆角比例：实测失败，回退任务书预估 {0}" -f $cornerFrac)
}

# ---------------------------------------------------------------------------
# ② 高质量缩放：从 (sx,sy,sw,sh) 源子矩形缩放到 (tw,th) 新位图。
# TileFlipXY 包裹模式是缩采样的经典防坑设置：不加它，DrawImage 在
# 边缘会采样到"图片外的空白"，产生一圈透明毛边。
# ---------------------------------------------------------------------------
function New-Step([System.Drawing.Bitmap]$bmp, [int]$sx, [int]$sy, [int]$sw, [int]$sh, [int]$tw, [int]$th) {
    $dst = New-Object System.Drawing.Bitmap($tw, $th, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($dst)
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode   = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.SmoothingMode     = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)
    $attr = New-Object System.Drawing.Imaging.ImageAttributes
    $attr.SetWrapMode([System.Drawing.Drawing2D.WrapMode]::TileFlipXY) | Out-Null
    $rect = New-Object System.Drawing.Rectangle(0, 0, $tw, $th)
    $g.DrawImage($bmp, $rect, $sx, $sy, $sw, $sh, [System.Drawing.GraphicsUnit]::Pixel, $attr)
    $attr.Dispose(); $g.Dispose()
    return $dst
}

# ---------------------------------------------------------------------------
# ③ 圆角透明蒙版（逐像素 SDF）：
# 圆角矩形 = "内缩矩形（半宽 inner = size/2 - R）" 与 "半径 R 的圆" 的
# 闵可夫斯基和，其符号距离 sd<0 在内部、sd>0 在外部；
# 覆盖率 cov = smoothstep(0.5 - sd/羽化宽)，蒙版外 alpha=0、边缘 1px 平滑。
# 同时把完全透明的像素 RGB 清零，避免个别渲染器出现彩色镶边。
# ---------------------------------------------------------------------------
function Apply-RoundedMask([System.Drawing.Bitmap]$bmp, [double]$radius, [double]$feather) {
    $size = $bmp.Width
    $rect = New-Object System.Drawing.Rectangle(0, 0, $size, $size)
    $fmt32 = [System.Drawing.Imaging.PixelFormat]::Format32bppArgb
    $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadWrite, $fmt32)
    $stride32 = $data.Stride
    $buf = New-Object byte[] ($stride32 * $size)
    [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $buf, 0, $buf.Length)

    $half  = $size / 2.0
    $inner = $half - $radius
    for ($y = 0; $y -lt $size; $y++) {
        $rowBase = $y * $stride32
        # 像素中心取 y+0.5：SDF 按连续坐标算，像素中心才是正确采样点
        $qy = [Math]::Abs($y + 0.5 - $half) - $inner
        for ($x = 0; $x -lt $size; $x++) {
            $qx = [Math]::Abs($x + 0.5 - $half) - $inner
            $mx = $qx; if ($mx -lt 0) { $mx = 0 }
            $my = $qy; if ($my -lt 0) { $my = 0 }
            $dOut = [Math]::Sqrt($mx * $mx + $my * $my)
            $mm = $qx; if ($qy -gt $mm) { $mm = $qy }
            $dIn = $mm; if ($dIn -gt 0) { $dIn = 0.0 }
            $sd = $dOut + $dIn - $radius           # 符号距离：0 恰好落在圆角轮廓上
            $t = 0.5 - $sd / $feather              # sd=-0.5feather→t=1 全不透明；sd=+0.5feather→t=0 全透明
            if ($t -lt 0) { $t = 0 } elseif ($t -gt 1) { $t = 1 }
            $cov = $t * $t * (3.0 - 2.0 * $t)      # smoothstep，比线性过渡更不容易看出台阶
            $i = $rowBase + $x * 4
            if ($cov -le 0.0) {
                $buf[$i] = 0; $buf[$i + 1] = 0; $buf[$i + 2] = 0; $buf[$i + 3] = 0
            } elseif ($cov -lt 1.0) {
                $buf[$i + 3] = [byte]([int]($buf[$i + 3] * $cov))
            }
        }
    }
    [System.Runtime.InteropServices.Marshal]::Copy($buf, 0, $data.Scan0, $buf.Length)
    $bmp.UnlockBits($data)
}

# ---------------------------------------------------------------------------
# 生成指定尺寸的成品位图：源图裁切框 → 逐级折半缩 → 最后 HighQualityBicubic
# 缩到 size×size → 加圆角透明蒙版。调用方负责 Dispose 返回值。
# ---------------------------------------------------------------------------
function New-LogoSquare([int]$size) {
    $cur = $srcBmp; $sx = $bbX0; $sy = $bbY0; $sw = $bbW; $sh = $bbH
    # 逐级对半：每次缩到约 1/2，直到与目标差距小于 2 倍为止（最稳的缩采样链）
    while ($sw -gt 2 * $size -or $sh -gt 2 * $size) {
        $nw = [int][Math]::Ceiling($sw / 2.0); if ($nw -lt $size) { $nw = $size }
        $nh = [int][Math]::Ceiling($sh / 2.0); if ($nh -lt $size) { $nh = $size }
        $next = New-Step $cur $sx $sy $sw $sh $nw $nh
        if ($cur -ne $srcBmp) { $cur.Dispose() }
        $cur = $next; $sx = 0; $sy = 0; $sw = $nw; $sh = $nh
    }
    $final = New-Step $cur $sx $sy $sw $sh $size $size
    if ($cur -ne $srcBmp) { $cur.Dispose() }
    $radius = $cornerFrac * $size
    Apply-RoundedMask $final $radius $FEATHER_PX
    return $final
}

# ---------------------------------------------------------------------------
# 把 Bitmap 编码成 PNG 字节（ICO 的每个条目就是一段完整 PNG 数据）。
# 返回前面加逗号是 PowerShell 的坑：不加字节数组会被"摊平"成一维。
# ---------------------------------------------------------------------------
function Get-PngBytes([System.Drawing.Bitmap]$bmp) {
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bytes = $ms.ToArray()
    $ms.Dispose()
    return ,$bytes
}

# ===========================================================================
# 主流程
# ===========================================================================
# ---- 1. 各尺寸 PNG 字节 ----
$pngs = @()
foreach ($sz in $SIZES) {
    $bmp = New-LogoSquare $sz
    $pngs += ,(Get-PngBytes $bmp)
    $bmp.Dispose()
    Write-Host ("  生成 {0}x{0} PNG（{1} 字节）" -f $sz, $pngs[-1].Length)
}

# ---- 2. 手工拼装 ICO 容器 ----
# 结构：ICONDIR 头（6 字节） + count × ICONDIRENTRY（每项 16 字节） + 各 PNG 数据。
#   ICONDIR:       reserved(u16)=0 | type(u16)=1(图标) | count(u16)
#   ICONDIRENTRY:  宽(u8) | 高(u8) | 颜色数(u8)=0 | 保留(u8)=0
#                  | planes(u16)=1 | bitCount(u16)=32 | 数据长度(u32) | 数据偏移(u32)
# 注意：256 这种"一个字节装不下"的尺寸，惯例宽高各写 0。条目按尺寸升序。
$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($ms)
$bw.Write([UInt16]0)                    # reserved
$bw.Write([UInt16]1)                    # type = 1（图标；光标是 2）
$bw.Write([UInt16]$SIZES.Count)         # 条目数
$offset = 6 + 16 * $SIZES.Count         # 第一张 PNG 数据的起始偏移
for ($i = 0; $i -lt $SIZES.Count; $i++) {
    $sz = $SIZES[$i]
    $dim = if ($sz -ge 256) { 0 } else { $sz }   # 256 写 0
    $bw.Write([Byte]$dim)                # 宽
    $bw.Write([Byte]$dim)                # 高
    $bw.Write([Byte]0)                   # 调色板颜色数（32 位真彩 = 0）
    $bw.Write([Byte]0)                   # 保留字节
    $bw.Write([UInt16]1)                 # planes（历史遗留，恒为 1）
    $bw.Write([UInt16]32)                # bitCount
    $bw.Write([UInt32]$pngs[$i].Length)  # 该条目 PNG 数据长度
    $bw.Write([UInt32]$offset)           # 该条目 PNG 数据偏移
    $offset += $pngs[$i].Length
}
foreach ($png in $pngs) { $bw.Write($png) }
$icoPath = Join-Path $assets "hellogate.ico"
[IO.File]::WriteAllBytes($icoPath, $ms.ToArray())
$bw.Dispose(); $ms.Dispose()
Write-Host ("ICO 已写入 {0}（{1} 字节）" -f $icoPath, (Get-Item $icoPath).Length)

# ---- 3. 512×512 带透明圆角 PNG（README 头图） ----
$png512 = New-LogoSquare 512
$pngPath = Join-Path $assets "hellogate.png"
$png512.Save($pngPath, [System.Drawing.Imaging.ImageFormat]::Png)
$png512.Dispose()
Write-Host ("PNG 已写入 {0}（{1} 字节）" -f $pngPath, (Get-Item $pngPath).Length)

$srcBmp.Dispose()

# ---- 4. 自验：重新解析生成的 ico，逐项校验 PNG 签名 ----
# PNG 文件魔数 8 字节：89 50 4E 47 0D 0A 1A 0A
$PNG_SIG = [byte[]](0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A)
$icoBytes = [IO.File]::ReadAllBytes($icoPath)
$rd = [BitConverter]::ToUInt16($icoBytes, 0); $tp = [BitConverter]::ToUInt16($icoBytes, 2)
$count = [BitConverter]::ToUInt16($icoBytes, 4)
Write-Host ("自验：reserved={0} type={1} count={2}" -f $rd, $tp, $count)
if ($rd -ne 0 -or $tp -ne 1) { throw "ICO 头校验失败（reserved/type 不对）" }
if ($count -ne $SIZES.Count) { throw "ICO 条目数不符（期望 $($SIZES.Count)，实际 $count）" }
$okAll = $true
for ($i = 0; $i -lt $count; $i++) {
    $e = 6 + 16 * $i
    $ew = $icoBytes[$e]; $eh = $icoBytes[$e + 1]
    # ICONDIRENTRY 内字段偏移：宽0 高1 颜色2 保留3 | planes@4 | bits@6 | len@8 | off@12
    $planes = [BitConverter]::ToUInt16($icoBytes, $e + 4)
    $bits   = [BitConverter]::ToUInt16($icoBytes, $e + 6)
    $len    = [BitConverter]::ToUInt32($icoBytes, $e + 8)
    $off    = [BitConverter]::ToUInt32($icoBytes, $e + 12)
    $dw = if ($ew -eq 0) { 256 } else { $ew }; $dh = if ($eh -eq 0) { 256 } else { $eh }
    $offI = [int]$off; $lenI = [int]$len
    $sig = $icoBytes[$offI..($offI + 7)]
    $sigOk = $true
    for ($k = 0; $k -lt 8; $k++) { if ($sig[$k] -ne $PNG_SIG[$k]) { $sigOk = $false } }
    $rangeOk = (($off + $len) -le $icoBytes.Length)
    if (-not $sigOk -or -not $rangeOk) { $okAll = $false }
    Write-Host ("  条目{0}: {1}x{1} planes={2} bits={3} len={4} off={5} PNG签名={6} 范围合法={7}" -f `
        $i, $dw, $planes, $bits, $len, $off, ($(if ($sigOk) { "89504E470D0A1A0A 正确" } else { "错误!" })), $rangeOk)
}
if (-not $okAll) { throw "ICO 自验未通过，检查上面条目明细" }
Write-Host "全部完成，ICO 自验通过。"
