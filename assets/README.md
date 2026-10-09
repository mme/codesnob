# Codesnob artwork

`codesnob-transparent.svg` is the editable source. The critic, wordmark and red
correction mark are separate groups of vector paths, with no embedded images
or font dependencies.

| Asset | Appearance |
| --- | --- |
| `codesnob-transparent.svg` / `.png` | Black lettering and illustration on transparency; the README's light-mode logo and fallback. |
| `codesnob-dark.svg` / `.png` | Light lettering and a paper-white sticker behind the black illustration, with transparency around it; the README's dark-mode logo. |
| `codesnob.svg` / `.png` | Black lettering and illustration on an opaque paper-white background. |

All variants are 2172 × 724. The README uses the SVGs at a width of 640 pixels;
the PNGs are native-size exports for places that need a raster image.

To regenerate the derived SVGs and all PNGs, install `rsvg-convert` (librsvg)
and run from the repository root:

```sh
uv run scripts/generate-assets.py
```

The script declares its Python dependencies. Edit the transparent source before
regenerating; edits made directly to the derived files will be overwritten.

The sticker follows the original generator recovered from Claude Code's logs,
used for commit `b6bec05`: render the critic at 4× resolution, expand its alpha
mask by 12 SVG units, fill enclosed areas, smooth the outline, and trace it back
to a vector path. The black illustration and red mark stay unchanged; only the
wordmark switches to light ink. PNGs are rendered with `rsvg-convert`.
