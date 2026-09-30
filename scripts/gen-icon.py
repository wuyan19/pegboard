#!/usr/bin/env python3
# 生成 assets/icon.png（主图）与 assets/icon-source.svg（矢量源）：
# Apple 风 squircle（超椭圆）蓝底 + 白色「孔 + 销」字形（比例取自 v0.1.1 图标实测值）。
#
# 对齐 Apple macOS 应用图标规范（同 serial-studio crates/tauri-app/gen-icon.py 方案）：
# - 1024 主图：icns 的 icon_512x512@2x 直接下采样取源（此前 512 主图被 sips
#   上采样到 1024，细节发虚）。
# - 四周 100px 透明边距，可见图形 824px（80%）：macOS 图标需此留白，满幅图标
#   在 Dock/Finder 中比系统图标显大（v0.1.1 的问题即此）。
# - 底形用真 squircle（超椭圆 n≈5）参数方程采样，非圆弧矩形——Apple 的连续
#   曲线圆角，圆弧矩形对不上。
#
# 渲染不走 SVG 栅格化（免 cairosvg/libcairo 原生依赖）：几何直接按行区间填充
# 4× 超采样遮罩，LANCZOS 降采样出 17 级抗锯齿；颜色与透明遮罩分离合成，
# 无透明边缘色偏。改参数重跑：python3 scripts/gen-icon.py。
# icns 由 scripts/package.sh 打包时用 iconutil/sips 就地生成，无需手工介入。
import math
from pathlib import Path

from PIL import Image

# ---- 设计参数（改这里，两处输出同步变化）----
SIZE = 1024            # 主图画布
PAD = 100              # 四周透明边距（macOS 规范 ~10%）
N = 5.0                # 超椭圆指数（Apple squircle 近似）
PTS = 256              # SVG 路径采样点数
SS = 4                 # 超采样倍数（抗锯齿质量）
BG = (37, 99, 235)     # #2563EB（沿用 v0.1.1 品牌蓝）
FG = (255, 255, 255)   # #FFFFFF

ART = SIZE - 2 * PAD   # 可见图形边长 824
A = ART / 2            # 超椭圆半轴 412
CX = CY = SIZE / 2

# 字形占 ART 的比例（v0.1.1 图标实测）：圆环 = 洞洞板的孔，圆点 = 插入的销
RING_OUTER_F = 135.5 / 512
RING_INNER_F = 82.5 / 512
DOT_F = 37.5 / 512


def squircle_half_width(dy: float) -> float:
    """超椭圆 |x/A|^n + |y/A|^n = 1 在纵坐标偏移 dy 处的半宽。"""
    t = abs(dy) / A
    if t >= 1.0:
        return 0.0
    return A * (1.0 - t**N) ** (1.0 / N)


def render_mask(runs_at, w: int, h: int, scale: float) -> Image.Image:
    """按行区间填充单通道遮罩：runs_at(dy) 以设计坐标返回该行 (lo, hi) x 区间；
    scale 为设计坐标 → 像素坐标的换算比。图形上下对称，填充一半行后镜像。"""
    buf = bytearray(w * h)
    for j in range(h // 2):
        runs = runs_at(((j + 0.5) - h / 2) / scale)
        if not runs:
            continue
        row = bytearray(w)
        for lo, hi in runs:
            x0 = max(0, math.ceil(lo * scale - 0.5 + w / 2))
            x1 = min(w - 1, math.floor(hi * scale + w / 2 - 0.5))
            if x1 >= x0:
                row[x0 : x1 + 1] = b"\xff" * (x1 + 1 - x0)
        buf[j * w : (j + 1) * w] = row
        buf[(h - 1 - j) * w : (h - j) * w] = row
    return Image.frombytes("L", (w, h), bytes(buf))


def main() -> None:
    out_dir = Path(__file__).resolve().parent.parent / "assets"
    ss = SIZE * SS
    r_out, r_in, r_dot = (ART * f for f in (RING_OUTER_F, RING_INNER_F, DOT_F))

    def bg_runs(dy: float):
        hw = squircle_half_width(dy)
        return [(-hw, hw)] if hw > 0 else []

    def glyph_runs(dy: float):
        runs = []
        for r in (r_out, r_in):  # 圆环外/内边界在同一行的半宽
            if abs(dy) < r:
                runs.append(math.sqrt(r * r - dy * dy))
        if len(runs) == 2:  # 环带：内边界之内挖空
            result = [(-runs[0], -runs[1]), (runs[1], runs[0])]
        elif len(runs) == 1:  # |dy| 越过内圈：整行横贯的环上段/下段
            result = [(-runs[0], runs[0])]
        else:
            result = []
        if abs(dy) < r_dot:  # 中心销钉（半径小于内圈，与环带区间不相交）
            hw = math.sqrt(r_dot * r_dot - dy * dy)
            result.append((-hw, hw))
        return result

    mask_bg = render_mask(bg_runs, ss, ss, SS).resize((SIZE, SIZE), Image.LANCZOS)
    mask_fg = render_mask(glyph_runs, ss, ss, SS).resize((SIZE, SIZE), Image.LANCZOS)

    base = Image.new("RGBA", (SIZE, SIZE), BG + (255,))
    glyph = Image.new("RGBA", (SIZE, SIZE), FG + (255,))
    glyph.putalpha(mask_fg)
    base.alpha_composite(glyph)  # straight-alpha 合成，字形边缘无色偏
    base.putalpha(mask_bg)
    base.save(out_dir / "icon.png")

    write_svg(out_dir / "icon-source.svg", r_out, r_in, r_dot)
    print(
        f"assets/icon.png: {SIZE}px 主图（art {ART}px + 边距 {PAD}px，squircle n={N}）；"
        f"assets/icon-source.svg 同步生成"
    )


def write_svg(path: Path, r_out: float, r_in: float, r_dot: float) -> None:
    parts = []
    for i in range(PTS):
        t = 2 * math.pi * i / PTS
        c, s = math.cos(t), math.sin(t)
        x = CX + A * math.copysign(abs(c) ** (2 / N), c)
        y = CY + A * math.copysign(abs(s) ** (2 / N), s)
        parts.append(f"{x:.1f} {y:.1f}")
    squircle = "M " + " L ".join(parts) + " Z"

    def circle(r: float, sweep: int) -> str:
        return f"M {CX - r:.1f} {CY:.1f} a {r:.1f} {r:.1f} 0 1 {sweep} {2 * r:.1f} 0 a {r:.1f} {r:.1f} 0 1 {sweep} {-2 * r:.1f} 0 Z"

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" width="{SIZE}" height="{SIZE}" viewBox="0 0 {SIZE} {SIZE}">
  <path d="{squircle}" fill="#%02x%02x%02x" />
  <path fill-rule="evenodd" fill="#%02x%02x%02x" d="{circle(r_out, 1)} {circle(r_in, 0)}" />
  <circle cx="{CX}" cy="{CY}" r="{r_dot:.1f}" fill="#%02x%02x%02x" />
</svg>
""" % (*BG, *FG, *FG)
    path.write_text(svg, encoding="utf-8")


if __name__ == "__main__":
    main()
