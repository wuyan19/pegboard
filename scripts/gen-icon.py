#!/usr/bin/env python3
# 生成 assets/icon.png（主图）与 assets/icon-source.svg（矢量源）：
# Apple 风 squircle 蓝底 + 白色「孔 + 销」字形（比例取自 v0.1.1 图标实测值）。
#
# 对齐 Apple macOS 应用图标规范（同 serial-studio crates/tauri-app/gen-icon.py 方案）：
# - 1024 主图：icns 的 icon_512x512@2x 直接下采样取源（此前 512 主图被 sips
#   上采样到 1024，细节发虚）。
# - 四周 100px 透明边距，可见图形 824px（80%）：macOS 图标需此留白，满幅图标
#   在 Dock/Finder 中比系统图标显大（v0.1.1 的问题即此）。
# - 底形用 Apple 标准连续曲率圆角（squircle）：平滑圆角矩形。直边 → 三次
#   贝塞尔 → 圆弧 → 三次贝塞尔拼接，曲率连续；既非圆弧矩形（G1 不连续），
#   也非整体超椭圆（无平直边、角部偏尖，与系统图标并排能看出差别）。
#   构造同 Figma「corner smoothing」（figma.com/blog/desperately-seeking-squircles；
#   算法按 MartinRGB/Figma_Squircles_Approximation 与 figma-squircle 的实现移植）。
#   参数按本机 macOS 系统图标实测（VoiceOver 等，遮罩 412px@512，alpha 阈值
#   亚像素边界最小二乘）：圆角半径 = 图形边长 × 0.225（824 → 185.4），平滑
#   系数 ξ = 0.6（Figma 匹配 Apple 图标的取值）；残差 rms ≈ 0.3px@512，即
#   Big Sur 以来 Apple 系统图标的标准底形。
#
# 渲染不走 SVG 栅格化（免 cairosvg/libcairo 原生依赖）：几何直接按行区间填充
# 4× 超采样遮罩，LANCZOS 降采样出 17 级抗锯齿；颜色与透明遮罩分离合成，
# 无透明边缘色偏。改参数重跑：python3 scripts/gen-icon.py。
# icns 由 scripts/package.sh 打包时用 iconutil/sips 就地生成，无需手工介入。
import math
from bisect import bisect_right
from pathlib import Path

from PIL import Image

# ---- 设计参数（改这里，两处输出同步变化）----
SIZE = 1024            # 主图画布
PAD = 100              # 四周透明边距（macOS 规范 ~10%）
SS = 4                 # 超采样倍数（抗锯齿质量）
BG = (37, 99, 235)     # #2563EB（沿用 v0.1.1 品牌蓝）
FG = (255, 255, 255)   # #FFFFFF

ART = SIZE - 2 * PAD   # 可见图形边长 824
A = ART / 2            # 图形半边长 412
CX = CY = SIZE / 2

# Apple 图标底形：平滑圆角矩形（本机系统图标实测拟合值，见文件头注释）
RADIUS_F = 0.225       # 圆角半径 / 图形边长（824 → 185.4）
SMOOTHING = 0.6        # 平滑系数 ξ：0=普通圆角矩形，0.6≈Apple 图标形状

# 字形占 ART 的比例（v0.1.1 图标实测）：圆环 = 洞洞板的孔，圆点 = 插入的销
RING_OUTER_F = 135.5 / 512
RING_INNER_F = 82.5 / 512
DOT_F = 37.5 / 512


class AppleSquircle:
    """Figma 式平滑圆角矩形（Apple 图标底形）。

    局部坐标系：角点为原点，x/y 沿两条边向图形内。单个圆角区的曲线：
    (p,0) —三次贝塞尔— (L+d, d) —圆弧— (d, d+L) —三次贝塞尔— (0, p)，
    与直边 G2 相接（起点控制点共线于边 → 起点曲率为 0）。
    """

    def __init__(self, half: float, radius: float, smoothing: float):
        self.half = half
        self.radius = radius
        self.smoothing = smoothing
        # 空间不足时按 Figma 的做法回退：压低平滑使圆角区不越过中线
        smoothing = min(smoothing, half / radius - 1.0)
        p = (1.0 + smoothing) * radius
        arc_measure = 90.0 * (1.0 - smoothing)
        self.arc_len = math.sin(math.radians(arc_measure) / 2.0) * radius * math.sqrt(2.0)
        angle_alpha = math.radians((90.0 - arc_measure) / 2.0)
        p3p4 = radius * math.tan(angle_alpha / 2.0)  # 控制点 P3、P4 间距
        angle_beta = math.radians(45.0 * smoothing)
        c = p3p4 * math.cos(angle_beta)
        d = c * math.tan(angle_beta)
        b = (p - self.arc_len - c - d) / 3.0
        a = 2.0 * b
        self.p, self.a, self.b, self.c, self.d = p, a, b, c, d

        # 采样圆角曲线为 (ly → lx) 查找表，行扫描时线性插值
        pts = [(p, 0.0)]
        pts += self._bezier(pts[-1], (p - a, 0.0), (p - a - b, 0.0), (p - a - b - c, d))
        s, e = (self.arc_len + d, d), (d, d + self.arc_len)  # 圆弧两端
        mid = ((s[0] + e[0]) / 2.0, (s[1] + e[1]) / 2.0)
        half_chord = math.hypot(e[0] - s[0], e[1] - s[1]) / 2.0
        if half_chord > 1e-9:  # ξ=1 时弧长为 0
            k = math.sqrt(max(radius * radius - half_chord * half_chord, 0.0))
            center = (mid[0] + k / math.sqrt(2.0), mid[1] + k / math.sqrt(2.0))
            a0 = math.atan2(s[1] - center[1], s[0] - center[0])
            a1 = math.atan2(e[1] - center[1], e[0] - center[0])
            if a1 > a0:
                a1 -= 2.0 * math.pi  # 凸向角点方向的短弧
            for i in range(1, 1001):
                ang = a0 + (a1 - a0) * i / 1000
                pts.append((center[0] + radius * math.cos(ang),
                            center[1] + radius * math.sin(ang)))
        pts += self._bezier(e, (0.0, e[1] + c), (0.0, e[1] + b + c), (0.0, p))
        self.lys = [q[1] for q in pts]
        self.lxs = [q[0] for q in pts]

    @staticmethod
    def _bezier(p0, p1, p2, p3, n=1000):
        out = []
        for i in range(1, n + 1):
            t = i / n
            mt = 1.0 - t
            out.append((mt**3 * p0[0] + 3 * mt * mt * t * p1[0] + 3 * mt * t * t * p2[0] + t**3 * p3[0],
                        mt**3 * p0[1] + 3 * mt * mt * t * p1[1] + 3 * mt * t * t * p2[1] + t**3 * p3[1]))
        return out

    def half_width(self, dy: float) -> float:
        """纵坐标偏移 dy 处的半宽（图形上下左右均对称）。"""
        ly = self.half - abs(dy)
        if ly < 0.0:
            return 0.0  # 图形外
        if ly >= self.p:
            return self.half  # 平直边
        i = bisect_right(self.lys, ly)
        if i == 0:
            lx = self.lxs[0]
        elif i >= len(self.lys):
            lx = self.lxs[-1]
        else:
            t = (ly - self.lys[i - 1]) / (self.lys[i] - self.lys[i - 1])
            lx = self.lxs[i - 1] + t * (self.lxs[i] - self.lxs[i - 1])
        return self.half - lx


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
    squircle = AppleSquircle(A, ART * RADIUS_F, SMOOTHING)

    def bg_runs(dy: float):
        hw = squircle.half_width(dy)
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

    write_svg(out_dir / "icon-source.svg", r_out, r_in, r_dot, squircle)
    print(
        f"assets/icon.png: {SIZE}px 主图（art {ART}px + 边距 {PAD}px，"
        f"Apple squircle R={ART * RADIUS_F:.1f} ξ={SMOOTHING}）；"
        f"assets/icon-source.svg 同步生成"
    )


def write_svg(path: Path, r_out: float, r_in: float, r_dot: float, squircle: AppleSquircle) -> None:
    # 四角曲线段与 AppleSquircle 的采样几何完全一致（figma-squircle 的路径拼装）
    a, b, c, d = squircle.a, squircle.b, squircle.c, squircle.d
    p = squircle.p
    R = squircle.radius
    L = squircle.arc_len
    lo, hi = PAD, PAD + ART

    def f(v: float) -> str:
        return f"{v:.2f}".rstrip("0").rstrip(".")

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" width="{SIZE}" height="{SIZE}" viewBox="0 0 {SIZE} {SIZE}">
  <path fill="#%02x%02x%02x" d="
    M {f(hi - p)} {f(lo)}
    c {f(a)} 0 {f(a + b)} 0 {f(a + b + c)} {f(d)}
    a {f(R)} {f(R)} 0 0 1 {f(L)} {f(L)}
    c {f(d)} {f(c)} {f(d)} {f(b + c)} {f(d)} {f(a + b + c)}
    L {f(hi)} {f(hi - p)}
    c 0 {f(a)} 0 {f(a + b)} {f(-d)} {f(a + b + c)}
    a {f(R)} {f(R)} 0 0 1 {f(-L)} {f(L)}
    c {f(-c)} {f(d)} {f(-(b + c))} {f(d)} {f(-(a + b + c))} {f(d)}
    L {f(lo + p)} {f(hi)}
    c {f(-a)} 0 {f(-(a + b))} 0 {f(-(a + b + c))} {f(-d)}
    a {f(R)} {f(R)} 0 0 1 {f(-L)} {f(-L)}
    c {f(-d)} {f(-c)} {f(-d)} {f(-(b + c))} {f(-d)} {f(-(a + b + c))}
    L {f(lo)} {f(lo + p)}
    c 0 {f(-a)} 0 {f(-(a + b))} {f(d)} {f(-(a + b + c))}
    a {f(R)} {f(R)} 0 0 1 {f(L)} {f(-L)}
    c {f(c)} {f(-d)} {f(b + c)} {f(-d)} {f(a + b + c)} {f(-d)}
    Z" />
  <path fill-rule="evenodd" fill="#%02x%02x%02x" d="{_circle(r_out, 1)} {_circle(r_in, 0)}" />
  <circle cx="{f(CX)}" cy="{f(CY)}" r="{f(r_dot)}" fill="#%02x%02x%02x" />
</svg>
""" % (*BG, *FG, *FG)
    path.write_text(svg, encoding="utf-8")


def _circle(r: float, sweep: int) -> str:
    return f"M {CX - r:.1f} {CY:.1f} a {r:.1f} {r:.1f} 0 1 {sweep} {2 * r:.1f} 0 a {r:.1f} {r:.1f} 0 1 {sweep} {-2 * r:.1f} 0 Z"


if __name__ == "__main__":
    main()
