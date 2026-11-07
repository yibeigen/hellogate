# ============================================================================
# HelloGate 品牌图标生成脚本（可重复执行，随时再生成）
#
# 【这个脚本干什么】
# 按品牌规格绘制「H 门柱 × 盾牌」Logo，输出两样东西：
#   1. assets\hellogate.ico —— 多尺寸 ICO（16/24/32/48/64/128/256），编进 exe；
#   2. assets\hellogate.png —— 512×512 PNG，README 头图用。
#
# 【Logo 几何规格】（下面所有尺寸都是"相对画布边长 S 的比例"，
#  这样任何分辨率下形状都一致，符合"矢量思路、位图输出"）
#   - 底板：圆角方块，圆角半径 = 0.22 × S；
#           垂直渐变 #6366F1（上）→ #4F46E5（下），亮靛蓝，不允许暗色背景；
#   - 盾牌：白色实心，宽 0.52S、高 0.58S，盾顶在画布 21% 高度处（光学居中偏上）；
#           顶边两角小圆角 0.08S；两侧垂直下探到盾高 55% 处，
#           再以二次贝塞尔收向底部中点尖；
#   - H 镂空：用渐变底色 #4F46E5 画在盾上（视觉上等于"挖空"）；
#           宽 0.30S、高 0.30S，竖笔/横笔粗细均 0.055S；
#           水平居中，垂直位于"盾高 30%~75% 区间"的中点（即盾顶 + 0.525×盾高）。
#
# 【清晰度策略】先按目标尺寸的 4 倍超采样绘制（AntiAlias 平滑），
# 再用 HighQualityBicubic 高质量缩采样到目标尺寸——小尺寸下边缘依然干净。
#
# 【运行方式】在项目根目录执行：powershell -File tools\make_icon.ps1
# ============================================================================

Add-Type -AssemblyName System.Drawing

# ---------------------------------------------------------------------------
# 品牌色（与 Rust 侧 ui.rs Palette::BRAND / BRAND_LIGHT 对应）
# ---------------------------------------------------------------------------
$BRAND_LIGHT = [System.Drawing.Color]::FromArgb(255, 0x63, 0x66, 0xF1) # #6366F1 渐变上端（亮靛蓝）
$BRAND_DARK  = [System.Drawing.Color]::FromArgb(255, 0x4F, 0x46, 0xE5) # #4F46E5 渐变下端 & H 镂空色
$WHITE       = [System.Drawing.Color]::FromArgb(255, 255, 255, 255)    # 盾牌纯白

# ---------------------------------------------------------------------------
# 几何参数（全部以"比例 × 边长 S"表达，改规格只改这几个系数）
# ---------------------------------------------------------------------------
$R_BASE    = 0.22   # 底板圆角半径 / S
$W_SHIELD  = 0.52   # 盾牌宽 / S
$H_SHIELD  = 0.58   # 盾牌高 / S
$TOP_SHIELD = 0.21  # 盾顶 y / S（光学居中：略高于几何中心）
$R_SHIELD_TOP = 0.08 # 盾顶两角圆角半径 / S
$STRAIGHT_FRAC = 0.55 # 两侧垂直段下探到"盾高"的这个比例后才开始收尖
$W_H       = 0.30   # H 宽 / S
$H_H       = 0.30   # H 高 / S
$T_H       = 0.055  # H 竖笔（=横笔）粗 / S
$H_CENTER_FRAC = 0.525 # H 垂直中心 = 盾顶 + 0.525 × 盾高（即盾高 30%~75% 区间的中点）

# ---------------------------------------------------------------------------
# 画一条二次贝塞尔（GraphicsPath 只提供三次贝塞尔，
# 这里按"升阶公式"把 二次(P0,Q,P2) 转成 三次(P0,C1,C2,P2)：
#   C1 = P0 + 2/3 (Q - P0)
#   C2 = P2 + 2/3 (Q - P2)
# 两种曲线在数学上完全同形，只是表达形式不同。
# ---------------------------------------------------------------------------
function Add-QuadBezier($path, $p0x, $p0y, $qx, $qy, $p2x, $p2y) {
    $c1x = $p0x + (2.0 / 3.0) * ($qx - $p0x)
    $c1y = $p0y + (2.0 / 3.0) * ($qy - $p0y)
    $c2x = $p2x + (2.0 / 3.0) * ($qx - $p2x)
    $c2y = $p2y + (2.0 / 3.0) * ($qy - $p2y)
    $path.AddBezier([double]$p0x, [double]$p0y,
                    [double]$c1x, [double]$c1y,
                    [double]$c2x, [double]$c2y,
                    [double]$p2x, [double]$p2y) | Out-Null
}

# ---------------------------------------------------------------------------
# 底板：圆角方块路径。
# GraphicsPath.AddArc(x, y, w, h, startAngle, sweepAngle)：
# 角度以"屏幕坐标（y 向下）"计，0°=3 点钟方向，正角度顺时针。
# 四个 90° 圆弧按顺时针首尾相接，就得到一个圆角矩形轮廓。
# ---------------------------------------------------------------------------
function New-BasePath($S) {
    $d = 2.0 * $R_BASE * $S   # 圆角直径（AddArc 的椭圆宽高）
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $p.AddArc(0.0, 0.0, $d, $d, 180.0, 90.0)              # 左上角
    $p.AddArc($S - $d, 0.0, $d, $d, 270.0, 90.0)          # 右上角
    $p.AddArc($S - $d, $S - $d, $d, $d, 0.0, 90.0)        # 右下角
    $p.AddArc(0.0, $S - $d, $d, $d, 90.0, 90.0)           # 左下角
    $p.CloseFigure() | Out-Null
    $p
}

# ---------------------------------------------------------------------------
# 盾牌轮廓路径（顺时针一圈）：
#   顶边（带两角小圆角）→ 两侧垂直下探到 55% 盾高 → 二次贝塞尔收向底部中点尖。
# 各段端点严格首尾重合，GraphicsPath 会自动把它们连成一条封闭轮廓。
# ---------------------------------------------------------------------------
function New-ShieldPath($S) {
    $sw = $W_SHIELD * $S; $sh = $H_SHIELD * $S
    $left = ($S - $sw) / 2.0; $right = $left + $sw
    $top = $TOP_SHIELD * $S; $bottom = $top + $sh
    $rt = $R_SHIELD_TOP * $S
    $ys = $top + $STRAIGHT_FRAC * $sh   # 垂直段终点（从这里开始收尖）
    $cx = $S / 2.0
    $d = 2.0 * $rt

    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    # ① 顶边左段：从左圆角终点画到右圆角起点
    $p.AddLine([double]($left + $rt), [double]$top, [double]($right - $rt), [double]$top) | Out-Null
    # ② 右上角圆弧：270°（正上方）顺时针扫 90° 到 0°（正右方）
    $p.AddArc([double]($right - $d), [double]$top, $d, $d, 270.0, 90.0) | Out-Null
    # ③ 右侧垂直下探
    $p.AddLine([double]$right, [double]($top + $rt), [double]$right, [double]$ys) | Out-Null
    # ④ 右半收尖：二次贝塞尔 P0=(right,ys) Q=(right,bottom) P2=(cx,bottom)
    #    控制点取"右下角"，曲线先贴着侧边继续向下、再平滑拐向尖端。
    Add-QuadBezier $p $right $ys $right $bottom $cx $bottom
    # ⑤ 左半收尖：镜像曲线 (cx,bottom) → (left,ys)
    Add-QuadBezier $p $cx $bottom $left $bottom $left $ys
    # ⑥ 左侧垂直上探
    $p.AddLine([double]$left, [double]$ys, [double]$left, [double]($top + $rt)) | Out-Null
    # ⑦ 左上角圆弧：180°（正左方）顺时针扫 90° 到 270°（正上方），回到起点
    $p.AddArc([double]$left, [double]$top, $d, $d, 180.0, 90.0) | Out-Null
    $p.CloseFigure() | Out-Null
    $p
}

# ---------------------------------------------------------------------------
# 在 Graphics 对象上绘制完整 Logo（$S = 画布边长，可以是超采样后的大值）
# ---------------------------------------------------------------------------
function Draw-Logo($g, $S) {
    # ---- ① 底板：垂直渐变圆角方块 ----
    # LinearGradientBrush 的 Vertical 模式 = 矩形上边缘取第一色、下边缘取第二色。
    $base = New-BasePath $S
    $grad = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
        (New-Object System.Drawing.Rectangle(0, 0, [int]$S, [int]$S)),
        $BRAND_LIGHT, $BRAND_DARK,
        [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)
    $g.FillPath($grad, $base)
    $grad.Dispose(); $base.Dispose()

    # ---- ② 盾牌：白色实心 ----
    $shield = New-ShieldPath $S
    $wb = New-Object System.Drawing.SolidBrush $WHITE
    $g.FillPath($wb, $shield)
    $wb.Dispose(); $shield.Dispose()

    # ---- ③ H 镂空：用渐变底色 #4F46E5 画三条矩形（两竖一横） ----
    # 视觉上等于把盾牌"挖"出一个 H，露出底下的渐变板色。
    # 两竖是门柱、一横是门梁——"门"的意象正是 HelloGate 的名字来源。
    $hw = $W_H * $S; $hh = $H_H * $S; $t = $T_H * $S
    $cx = $S / 2.0
    $sh = $H_SHIELD * $S; $top = $TOP_SHIELD * $S
    $hcy = $top + $H_CENTER_FRAC * $sh   # H 的垂直中心
    $hb = New-Object System.Drawing.SolidBrush $BRAND_DARK
    # 左竖（门柱）
    $g.FillRectangle($hb, [float]($cx - $hw / 2.0), [float]($hcy - $hh / 2.0), [float]$t, [float]$hh)
    # 右竖（门柱）
    $g.FillRectangle($hb, [float]($cx + $hw / 2.0 - $t), [float]($hcy - $hh / 2.0), [float]$t, [float]$hh)
    # 横（门梁）：垂直居中于 H 高度，横跨整个 H 宽度
    $g.FillRectangle($hb, [float]($cx - $hw / 2.0), [float]($hcy - $t / 2.0), [float]$hw, [float]$t)
    $hb.Dispose()
}

# ---------------------------------------------------------------------------
# 生成指定尺寸的 Logo 位图：4× 超采样绘制 → HighQualityBicubic 缩采样。
# ImageAttributes.SetWrapMode(TileFlipXY) 是缩采样的经典防坑设置：
# 不加它，DrawImage 在边缘会采样到"图片外的空白"，产生一圈透明毛边。
# 调用方负责 Dispose 返回的 Bitmap。
# ---------------------------------------------------------------------------
function New-LogoBitmap([int]$size) {
    $ss = $size * 4   # 超采样边长
    $big = New-Object System.Drawing.Bitmap($ss, $ss, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $gb = [System.Drawing.Graphics]::FromImage($big)
    $gb.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $gb.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $gb.Clear([System.Drawing.Color]::Transparent)
    Draw-Logo $gb ([double]$ss)
    $gb.Dispose()

    if ($size -eq $ss) { return $big }   # 理论上不会发生（恒有 4 倍），防御性写法

    $dst = New-Object System.Drawing.Bitmap($size, $size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $dg = [System.Drawing.Graphics]::FromImage($dst)
    $dg.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $dg.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $dg.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::HighQuality
    $dg.Clear([System.Drawing.Color]::Transparent)
    $attr = New-Object System.Drawing.Imaging.ImageAttributes
    $attr.SetWrapMode([System.Drawing.Drawing2D.WrapMode]::TileFlipXY) | Out-Null
    $rect = New-Object System.Drawing.Rectangle(0, 0, $size, $size)
    $dg.DrawImage($big, $rect, 0, 0, $ss, $ss, [System.Drawing.GraphicsUnit]::Pixel, $attr)
    $attr.Dispose(); $dg.Dispose(); $big.Dispose()
    return $dst
}

# ---------------------------------------------------------------------------
# 把 Bitmap 编码成 PNG 字节（ICO 的每个条目就是一段完整的 PNG 数据）。
# Vista+ 的 ICO 容器允许 PNG 压缩条目，目标平台 Win10/11 完全支持。
# ---------------------------------------------------------------------------
function Get-PngBytes($bmp) {
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $bytes = $ms.ToArray()
    $ms.Dispose()
    return ,$bytes
}

# ===========================================================================
# 主流程
# ===========================================================================
$root    = Split-Path -Parent $PSScriptRoot   # 项目根目录（tools 的上一级）
$assets  = Join-Path $root "assets"
if (-not (Test-Path $assets)) { New-Item -ItemType Directory -Path $assets | Out-Null }

# ---- 1. 多尺寸 ICO ----
# 尺寸序列：任务栏/托盘小图标(16/24/32)、桌面/资源管理器(48)、高 DPI(64)、
# 大图标视图(128)、最大尺寸(256)。条目按尺寸升序排列（惯例，非强制）。
$sizes = @(16, 24, 32, 48, 64, 128, 256)
$pngs = @()
foreach ($sz in $sizes) {
    $bmp = New-LogoBitmap $sz
    $pngs += ,(Get-PngBytes $bmp)   # 前面的逗号是 PowerShell 的坑：不加会被"摊平"成一维字节数组
    $bmp.Dispose()
    Write-Host ("  生成 {0}x{0} PNG（{1} 字节）" -f $sz, $pngs[-1].Length)
}

# ---- 2. 手工拼装 ICO 容器 ----
# 结构：ICONDIR 头（6 字节） + count × ICONDIRENTRY（每项 16 字节） + 各 PNG 数据。
#   ICONDIR:       reserved(u16)=0 | type(u16)=1(图标) | count(u16)
#   ICONDIRENTRY:  宽(u8) | 高(u8) | 颜色数(u8)=0 | 保留(u8)=0
#                  | planes(u16)=1 | bitCount(u16)=32 | 数据长度(u32) | 数据偏移(u32)
# 注意：256 这种"超过一个字节能表示的最大值 255"的尺寸，惯例是写 0。
$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($ms)
$bw.Write([UInt16]0)                    # reserved
$bw.Write([UInt16]1)                    # type = 1（图标；光标是 2）
$bw.Write([UInt16]$sizes.Count)         # 条目数
$offset = 6 + 16 * $sizes.Count         # 第一张 PNG 数据的起始偏移
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $sz = $sizes[$i]
    $dim = if ($sz -ge 256) { 0 } else { $sz }   # 256 写 0（宽高各 1 字节装不下 256）
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

# ---- 3. 512×512 PNG（README 头图） ----
$png512 = New-LogoBitmap 512
$pngPath = Join-Path $assets "hellogate.png"
$png512.Save($pngPath, [System.Drawing.Imaging.ImageFormat]::Png)
$png512.Dispose()
Write-Host ("PNG 已写入 {0}（{1} 字节）" -f $pngPath, (Get-Item $pngPath).Length)

Write-Host "全部完成。"
