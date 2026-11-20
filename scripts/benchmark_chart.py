"""Writes assets/benchmark.svg from the numbers measured by scripts/compare.ps1.

    python scripts/benchmark_chart.py

Small multiples: each metric has its own scale (MB and ms never share an axis).
Colors were checked with the dataviz palette validator against GitHub's light
(#ffffff) and dark (#0d1117) page backgrounds; the SVG follows the reader's theme.
"""
from pathlib import Path

# Median of 3 runs, Gitust repository open, AMD Ryzen 7 5700X, Windows 11.
METRICS = [
    ("Memory · private bytes", "MB", 267.3, 10.1, "less"),
    ("Memory · working set", "MB", 365.8, 32.7, "less"),
    ("Size on disk", "MB", 582.4, 16.5, "smaller"),
    ("Window visible after launch", "ms", 561, 50, "faster"),
]
APPS = ("GitHub Desktop 3.6.7", "Gitust 0.1.0")

W, PANEL_W, PANEL_H = 840, 400, 104
LEFT, LABEL_W, BAR_MAX = 20, 150, 170
BAR_H, ROW_GAP = 16, 10
TOP = 44


def bar(x, y, w, h, cls):
    """Bar anchored at the baseline (square left), 4px rounded data end (right)."""
    r = min(4, w / 2, h / 2)
    return (
        f'<path class="{cls}" d="M{x:.1f},{y:.1f}h{w - r:.1f}a{r},{r} 0 0 1 {r},{r}'
        f'v{h - 2 * r:.1f}a{r},{r} 0 0 1 -{r},{r}h-{w - r:.1f}z"/>'
    )


def fmt(v, unit):
    return f"{v:,.0f} {unit}" if v >= 100 or unit == "ms" else f"{v:.1f} {unit}"


out = []
rows = (len(METRICS) + 1) // 2
H = TOP + rows * PANEL_H + 26
out.append(
    f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" '
    'font-family="-apple-system,BlinkMacSystemFont,Segoe UI,Helvetica,Arial,sans-serif" role="img" '
    'aria-label="Gitust versus GitHub Desktop: memory, size on disk and startup time">'
)
out.append(
    """<style>
  .bg{fill:#ffffff} .t1{fill:#1f2328} .t2{fill:#59636e} .grid{stroke:#d1d9e0}
  .gd{fill:#4a7fb8} .gu{fill:#e8611f}
  @media (prefers-color-scheme: dark){
    .bg{fill:#0d1117} .t1{fill:#e6edf3} .t2{fill:#9198a1} .grid{stroke:#3d444d}
    .gd{fill:#5b93d1} .gu{fill:#e8611f}
  }
  text{font-size:13px}
  .title{font-weight:600} .small{font-size:12px} .big{font-weight:700}
</style>"""
)
out.append(f'<rect class="bg" width="{W}" height="{H}" rx="8"/>')

# Legend (two series): always present, swatch + name in text ink.
lx = LEFT
for name, cls in ((APPS[0], "gd"), (APPS[1], "gu")):
    out.append(f'<rect class="{cls}" x="{lx}" y="16" width="12" height="12" rx="3"/>')
    out.append(f'<text class="t1" x="{lx + 18}" y="26.5">{name}</text>')
    lx += 18 + len(name) * 7.4 + 24
out.append(f'<text class="t2 small" x="{W - LEFT}" y="26.5" text-anchor="end">Lower is better · median of 3 runs</text>')

for i, (title, unit, desktop, gitust, word) in enumerate(METRICS):
    px = LEFT + (i % 2) * (PANEL_W + 0)
    py = TOP + (i // 2) * PANEL_H
    ratio = desktop / gitust
    out.append(f'<text class="t1 title" x="{px}" y="{py + 16}">{title}</text>')
    out.append(f'<text class="t1 big" x="{px + PANEL_W - 30}" y="{py + 16}" text-anchor="end">{ratio:.0f}× {word}</text>')
    base_x = px + LABEL_W
    y0 = py + 30
    out.append(f'<line class="grid" x1="{base_x}" x2="{base_x}" y1="{y0 - 4}" y2="{y0 + 2 * BAR_H + ROW_GAP + 4}" stroke-width="1"/>')
    for j, (value, name, cls) in enumerate(((desktop, APPS[0], "gd"), (gitust, APPS[1], "gu"))):
        y = y0 + j * (BAR_H + ROW_GAP)
        w = max(3.0, BAR_MAX * value / desktop)
        out.append(f'<text class="t2" x="{base_x - 10}" y="{y + 12.5}" text-anchor="end">{name.split(" ")[0] + (" Desktop" if "Desktop" in name else "")}</text>')
        out.append(bar(base_x, y, w, BAR_H, cls))
        out.append(f'<text class="t1" x="{base_x + w + 8}" y="{y + 12.5}">{fmt(value, unit)}</text>')

out.append(
    f'<text class="t2 small" x="{LEFT}" y="{H - 12}">Windows 11 · AMD Ryzen 7 5700X · 32 GB · both apps idle on the same repository, '
    'all processes summed · scripts/compare.ps1</text>'
)
out.append("</svg>")
Path(__file__).resolve().parent.parent.joinpath("assets", "benchmark.svg").write_text("\n".join(out) + "\n", encoding="utf-8", newline="\n")
print("wrote assets/benchmark.svg")
