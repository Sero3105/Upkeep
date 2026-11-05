#!/bin/sh
# Regenerate README images: TUI screenshots (demo data) + logo, SVG → PNG at 2x.
# Uses only macOS built-ins (QuickLook + sips).
set -eu
cd "$(dirname "$0")/.."
UPKEEP_HOME="$(mktemp -d)" cargo test --quiet screenshots -- --ignored >/dev/null
tmp="$(mktemp -d)"
for svg in assets/screens/*.svg assets/logo.svg; do
  [ -f "$svg" ] || continue
  # SVGs are square with content centered (QuickLook scales those predictably); data-crop = height to keep.
  w=$(sed -n 's/.*<svg[^>]* width="\([0-9.]*\)".*/\1/p' "$svg" | head -1 | cut -d. -f1)
  h=$(sed -n 's/.*data-crop="\([0-9.]*\)".*/\1/p' "$svg" | head -1 | cut -d. -f1)
  h=${h:-$w}
  qlmanage -t -s $((w * 2)) -o "$tmp" "$svg" >/dev/null 2>&1
  sips -c $((h * 2)) $((w * 2)) "$tmp/$(basename "$svg").png" --out "${svg%.svg}.png" >/dev/null
  case "$svg" in
    # QuickLook has no alpha: cut the logo's rounded corners back to transparent (rx = 114/512).
    *logo.svg) python3 -c 'import sys
from PIL import Image, ImageDraw
im = Image.open(sys.argv[1]).convert("RGBA"); w = im.width
m = Image.new("L", im.size, 0); ImageDraw.Draw(m).rounded_rectangle((0, 0, w - 1, w - 1), radius=w * 114 // 512, fill=255)
im.putalpha(m); im.save(sys.argv[1])' "${svg%.svg}.png" ;;
  esac
  echo "${svg%.svg}.png"
done
rm -rf "$tmp"
