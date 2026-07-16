# Provider logo sources

The SVG masters in this directory are pinned from
[`lobehub/lobe-icons`](https://github.com/lobehub/lobe-icons) commit
`49ea6caba8ca1fadd56e7bc918cddcdc1f05aae1`:

- `packages/static-svg/icons/claude-color.svg`
- `packages/static-svg/icons/openai.svg`
- `packages/static-svg/icons/antigravity-color.svg`

Lobe Icons is distributed under the MIT License. The provider marks remain
trademarks of their respective owners. Generated PNG files are produced by
`tools/generate_provider_logos.mjs` with a local Chromium renderer, then
downsampled from 8x with premultiplied-alpha Lanczos filtering. Each output is
a complete provider tile (background, border, and mark) at one exact Windows
DPI bucket; do not resize it at runtime.

Generation requires Node.js, Playwright, Pillow, and a local Chromium-family
browser. Set `PLAYWRIGHT_MODULE_PATH` and `CHROMIUM_PATH` to those installed
locations before running the generator. `PYTHON_PATH` is optional and defaults
to `python` on Windows or `python3` elsewhere.
