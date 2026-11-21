"""Writes assets/benchmark.svg from the numbers measured by scripts/compare.ps1.

    python scripts/benchmark_chart.py

One card per metric, each with its own scale (MB and ms never share an axis) and
the headline ratio in large type. Bars grow in once when the image loads (CSS
animation; renderers without animation show the final chart). Colors were
checked with the dataviz palette validator against GitHub's light (#ffffff) and
dark (#0d1117) page backgrounds; the SVG follows the reader's theme.
"""
from pathlib import Path

# Median of 3 runs, Gitust repository open, AMD Ryzen 7 5700X, Windows 11.
METRICS = [
    ("Memory", "private bytes", "MB", 267.3, 10.1, "less memory"),
    ("Memory", "working set", "MB", 365.8, 32.7, "less memory"),
    ("Size on disk", "app folder vs. one .exe", "MB", 582.4, 16.5, "smaller"),
    ("Startup", "window visible after launch", "ms", 561, 50, "faster"),
]
APPS = ("GitHub Desktop 3.6.7", "Gitust 0.1.0")
SHORT = ("GitHub Desktop", "Gitust")

W = 880
PAD = 20                      # outer padding
GAP = 16                      # gap between cards
CARD_W = (W - 2 * PAD - GAP) // 2
CARD_H = 172
TOP = 58                      # first card's y
INNER = 22                    # card padding
LABEL_W = 108                 # row label column
BAR_H = 22
ROW_GAP = 12


def bar(x, y, w, h, cls, delay):
    """Bar anchored at the baseline (square left), rounded data end (right)."""
    r = min(5, w / 2, h / 2)
    return (
        f'<path class="bar {cls}" style="animation-delay:{delay:.2f}s" '
        f'd="M{x:.1f},{y:.1f}h{w - r:.1f}a{r},{r} 0 0 1 {r},{r}'
        f'v{h - 2 * r:.1f}a{r},{r} 0 0 1 -{r},{r}h-{w - r:.1f}z"/>'
    )


def fmt(v, unit):
    return f"{v:,.0f} {unit}" if v >= 100 or unit == "ms" else f"{v:.1f} {unit}"


rows = (len(METRICS) + 1) // 2
H = TOP + rows * CARD_H + (rows - 1) * GAP + 44
out = [
    f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}" '
    'font-family="-apple-system,BlinkMacSystemFont,Segoe UI,Helvetica,Arial,sans-serif" role="img" '
    'aria-label="Gitust versus GitHub Desktop: memory, size on disk and startup time">',
    """<style>
  .bg{fill:#ffffff} .card{fill:#f6f8fa;stroke:#d1d9e0} .t1{fill:#1f2328} .t2{fill:#59636e}
  .track{fill:#e7ecf0} .gd{fill:#4a7fb8} .gu{fill:#e8611f} .hero{fill:#d4561a}
  @media (prefers-color-scheme: dark){
    .bg{fill:#0d1117} .card{fill:#151b23;stroke:#30363d} .t1{fill:#e6edf3} .t2{fill:#9198a1}
    .track{fill:#212830} .gd{fill:#5b93d1} .gu{fill:#f0702c} .hero{fill:#f58a4e}
  }
  text{font-size:13px}
  .name{font-size:15px;font-weight:600} .sub{font-size:12px}
  .ratio{font-size:38px;font-weight:800;letter-spacing:-1px} .word{font-size:15px;font-weight:600}
  .val{font-size:13px;font-weight:600} .small{font-size:12px} .legend{font-size:13px;font-weight:600}
  .bar{transform-box:fill-box;transform-origin:left center;animation:grow .9s cubic-bezier(.2,.8,.2,1) backwards}
  .fade{animation:fade .5s ease-out backwards}
  @keyframes grow{from{transform:scaleX(0)}}
  @keyframes fade{from{opacity:0}}
  @media (prefers-reduced-motion: reduce){.bar,.fade{animation:none}}
</style>""",
    f'<rect class="bg" width="{W}" height="{H}" rx="12"/>',
]

# Legend: swatch + full app name; note on the right.
lx = PAD
for name, cls in zip(APPS, ("gd", "gu")):
    out.append(f'<rect class="{cls}" x="{lx}" y="21" width="14" height="14" rx="4"/>')
    out.append(f'<text class="t1 legend" x="{lx + 22}" y="32.5">{name}</text>')
    lx += 22 + len(name) * 7.8 + 28
out.append(f'<text class="t2 small" x="{W - PAD}" y="32.5" text-anchor="end">Lower is better · median of 3 runs</text>')

for i, (name, sub, unit, desktop, gitust, word) in enumerate(METRICS):
    cx = PAD + (i % 2) * (CARD_W + GAP)
    cy = TOP + (i // 2) * (CARD_H + GAP)
    out.append(f'<rect class="card" x="{cx + .5}" y="{cy + .5}" width="{CARD_W - 1}" height="{CARD_H - 1}" rx="10"/>')
    x0 = cx + INNER

    # Header: metric name + detail on the left, headline ratio on the right.
    out.append(f'<text class="t1 name" x="{x0}" y="{cy + 34}">{name}</text>')
    out.append(f'<text class="t2 sub" x="{x0}" y="{cy + 53}">{sub}</text>')
    ratio = f"{desktop / gitust:.0f}×"
    rx = cx + CARD_W - INNER
    out.append(f'<text class="t2 word fade" style="animation-delay:{.5 + i * .12:.2f}s" x="{rx}" y="{cy + 64}" text-anchor="end">{word}</text>')
    out.append(f'<text class="hero ratio fade" style="animation-delay:{.4 + i * .12:.2f}s" x="{rx}" y="{cy + 42}" text-anchor="end">{ratio}</text>')

    # Two bars on a shared track, scaled to this card's larger value.
    bx = x0 + LABEL_W
    bar_max = CARD_W - 2 * INNER - LABEL_W - 78
    y0 = cy + 88
    for j, (value, label, cls) in enumerate(((desktop, SHORT[0], "gd"), (gitust, SHORT[1], "gu"))):
        y = y0 + j * (BAR_H + ROW_GAP)
        w = max(4.0, bar_max * value / desktop)
        out.append(f'<text class="t2" x="{x0}" y="{y + 15.5}">{label}</text>')
        out.append(f'<rect class="track" x="{bx}" y="{y}" width="{bar_max}" height="{BAR_H}" rx="5"/>')
        out.append(bar(bx, y, w, BAR_H, cls, .1 + i * .12 + j * .08))
        out.append(f'<text class="t1 val" x="{bx + bar_max + 10}" y="{y + 15.5}">{fmt(value, unit)}</text>')

out.append(
    f'<text class="t2 small" x="{PAD}" y="{H - 16}">Windows 11 · AMD Ryzen 7 5700X · 32 GB · both apps idle on the same repository, '
    'all processes summed · scripts/compare.ps1</text>'
)
out.append("</svg>")
Path(__file__).resolve().parent.parent.joinpath("assets", "benchmark.svg").write_text("\n".join(out) + "\n", encoding="utf-8", newline="\n")
print("wrote assets/benchmark.svg")
