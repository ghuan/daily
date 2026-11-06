# -*- coding: utf-8 -*-
"""Generate 小铃 (Bell) brand assets: icon.ico / icon.png / notify.wav."""
import math
import os
import struct

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))

TILE = (78, 87, 210, 255)       # 品牌靛蓝
TILE_DEEP = (56, 64, 170, 255)  # 日历表头深靛蓝
WHITE = (255, 255, 255, 255)
BELL = (255, 205, 92, 255)      # 铃铛金色
BELL_DEEP = (222, 162, 36, 255)  # 铃舌 / 提环
DOT = (150, 158, 220, 255)      # 日期点阵


def draw_icon(size: int) -> Image.Image:
    """Indigo rounded tile: white calendar sheet + golden bell badge.

    小尺寸（<= 32px）省略日期点阵与提环，避免缩小后糊成一团。
    """
    detailed = size >= 48
    s = size * 6  # 超采样，缩小后边缘干净
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    pad = s // 16
    d.rounded_rectangle([pad, pad, s - pad, s - pad], radius=s // 4, fill=TILE)

    # 顶部高光，让底板有材质感
    hi = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    ImageDraw.Draw(hi).rounded_rectangle(
        [pad, pad, s - pad, int(s * 0.52)], radius=s // 4, fill=(255, 255, 255, 26)
    )
    img.alpha_composite(hi)

    # ---- 日历卡 ----
    cl, ct = int(s * 0.180), int(s * 0.240)
    cr, cb = int(s * 0.720), int(s * 0.770)
    crd = max(s // 22, 1)
    d.rounded_rectangle([cl, ct, cr, cb], radius=crd, fill=WHITE)

    # 表头色带：上圆角 + 下直角
    head_h = int((cb - ct) * 0.25)
    d.rounded_rectangle([cl, ct, cr, ct + head_h + crd], radius=crd, fill=TILE_DEEP)
    d.rectangle([cl, ct + head_h, cr, ct + head_h + crd], fill=TILE_DEEP)

    # 两个装订环
    ring_w = max(s // 48, 1)
    for fx in (0.33, 0.66):
        rx = cl + (cr - cl) * fx
        d.rounded_rectangle(
            [rx - ring_w, ct - int(s * 0.042), rx + ring_w, ct + int(s * 0.022)],
            radius=ring_w,
            fill=WHITE,
        )

    # 日期点阵：右下角留空给铃铛（小尺寸不画，否则缩略后是脏点）
    if detailed:
        dot = int((cr - cl) * 0.100)
        gap_x = (cr - cl) * 0.245
        gap_y = (cb - ct) * 0.215
        x0 = cl + (cr - cl) * 0.150
        y0 = ct + head_h + (cb - ct) * 0.180
        for r in range(2):
            for c in range(3):
                if r == 1 and c == 2:
                    continue
                cx = x0 + c * gap_x
                cy = y0 + r * gap_y
                d.rounded_rectangle(
                    [cx - dot / 2, cy - dot / 2, cx + dot / 2, cy + dot / 2],
                    radius=dot // 3,
                    fill=DOT,
                )

    # ---- 金色铃铛（右下角浮出） ----
    bx, by = int(s * 0.725), int(s * 0.735)
    bw = int(s * 0.165)  # 半宽

    # 铃铛底衬：用底板色挖出一圈，让它从日历上"浮"起来
    halo = int(bw * 1.50)
    d.ellipse([bx - halo, by - halo, bx + halo, by + halo], fill=TILE)

    body_top = by - bw
    body_bottom = by + int(bw * 0.66)

    # 铃身：上半圆 + 直筒
    d.pieslice([bx - bw, body_top, bx + bw, body_top + 2 * bw], start=180, end=360, fill=BELL)
    d.rectangle([bx - bw, body_top + bw, bx + bw, body_bottom], fill=BELL)
    # 下摆外扩
    d.ellipse(
        [
            bx - int(bw * 1.20),
            body_bottom - int(bw * 0.32),
            bx + int(bw * 1.20),
            body_bottom + int(bw * 0.32),
        ],
        fill=BELL,
    )

    if detailed:
        # 顶部提环
        d.ellipse(
            [
                bx - int(bw * 0.26),
                body_top - int(bw * 0.36),
                bx + int(bw * 0.26),
                body_top + int(bw * 0.20),
            ],
            fill=BELL_DEEP,
        )
    # 铃舌
    d.ellipse(
        [
            bx - int(bw * 0.30),
            body_bottom - int(bw * 0.04),
            bx + int(bw * 0.30),
            body_bottom + int(bw * 0.56),
        ],
        fill=BELL_DEEP,
    )

    return img.resize((size, size), Image.LANCZOS)


def gen_icons() -> None:
    png_path = os.path.join(HERE, "icon.png")
    ico_path = os.path.join(HERE, "icon.ico")
    sizes = (16, 24, 32, 48, 64, 128, 256)
    imgs = [draw_icon(sz) for sz in sizes]
    imgs[-1].save(png_path)
    imgs[-1].save(
        ico_path,
        format="ICO",
        sizes=[(i.width, i.height) for i in imgs],
        append_images=imgs[:-1],
    )
    print("icons ->", png_path, ico_path)


def gen_wav() -> None:
    """Two-note bell sound (880Hz -> 660Hz), 0.6s mono PCM."""
    rate = 22050
    notes = [(880.0, 0.0, 0.28), (660.0, 0.30, 0.60)]
    n = int(rate * 0.60)
    frames = bytearray()
    for i in range(n):
        t = i / rate
        sample = 0.0
        for freq, t0, t1 in notes:
            if t0 <= t < t1:
                env = min(1.0, (t - t0) / 0.01) * max(0.0, (t1 - t) / 0.12)
                sample += math.sin(2 * math.pi * freq * (t - t0)) * 0.6 * env
        v = int(max(-1.0, min(1.0, sample)) * 32767)
        frames += struct.pack("<h", v)
    data = bytes(frames)
    header = (
        b"RIFF" + struct.pack("<I", 36 + len(data)) + b"WAVE"
        + b"fmt " + struct.pack("<IHHIIHH", 16, 1, 1, rate, rate * 2, 2, 16)
        + b"data" + struct.pack("<I", len(data))
    )
    path = os.path.join(HERE, "notify.wav")
    with open(path, "wb") as f:
        f.write(header + data)
    print("wav ->", path, len(header) + len(data), "bytes")


if __name__ == "__main__":
    gen_icons()
    gen_wav()
