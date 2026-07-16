"""Downsample a provider tile with premultiplied-alpha Lanczos filtering."""

from __future__ import annotations

import argparse
from pathlib import Path

from PIL import Image


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("source", type=Path)
    parser.add_argument("destination", type=Path)
    parser.add_argument("size", type=int)
    parser.add_argument("supersample", type=int)
    args = parser.parse_args()

    if args.size <= 0:
        raise ValueError("size must be positive")
    if args.supersample <= 1:
        raise ValueError("supersample must be greater than one")

    with Image.open(args.source) as source:
        rgba = source.convert("RGBA")
        expected_source_size = args.size * args.supersample
        if rgba.size != (expected_source_size, expected_source_size):
            raise ValueError(
                f"source tile must be {expected_source_size}px square: {rgba.size}"
            )

        # Resize premultiplied channels so transparent pixels cannot leak dark
        # or colored fringes into the antialiased rounded corners.
        premultiplied = rgba.convert("RGBa")
        resized = premultiplied.resize(
            (args.size, args.size),
            Image.Resampling.LANCZOS,
        ).convert("RGBA")

        alpha_levels = len(set(resized.getchannel("A").get_flattened_data()))
        opaque_colors = len(
            {
                pixel[:3]
                for pixel in resized.get_flattened_data()
                if pixel[3] == 255
            }
        )
        # A native 16 px chip has fewer corner samples than the 20/28 px
        # classes, so 11 distinct alpha levels is its geometric maximum for
        # the current radius. Keep a strict, size-aware floor instead of
        # rejecting a correctly antialiased small tile with the 28 px rule.
        minimum_alpha_levels = 11 if args.size <= 16 else 12
        if alpha_levels < minimum_alpha_levels or opaque_colors < 100:
            raise ValueError(
                "downsampled tile lost antialiasing detail: "
                f"alpha_levels={alpha_levels}, opaque_colors={opaque_colors}"
            )

    args.destination.parent.mkdir(parents=True, exist_ok=True)
    resized.save(args.destination, format="PNG", optimize=True)


if __name__ == "__main__":
    main()
