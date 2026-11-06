#!/usr/bin/env python3
"""Render data/powerlimit.svg to PNGs in data/icons/hicolor/<n>x<n>/apps (needs python3-gi + librsvg)."""
import os
import gi
gi.require_version("GdkPixbuf", "2.0")
from gi.repository import GdkPixbuf

root = os.path.join(os.path.dirname(__file__), "..")
svg = os.path.join(root, "data", "powerlimit.svg")
for n in (16, 32, 48, 64, 128, 256, 512):
    d = os.path.join(root, "data", "icons", "hicolor", f"{n}x{n}", "apps")
    os.makedirs(d, exist_ok=True)
    GdkPixbuf.Pixbuf.new_from_file_at_size(svg, n, n).savev(os.path.join(d, "powerlimit.png"), "png", [], [])
os.makedirs(os.path.join(root, "docs"), exist_ok=True)
GdkPixbuf.Pixbuf.new_from_file_at_size(svg, 512, 512).savev(os.path.join(root, "docs", "icon.png"), "png", [], [])
