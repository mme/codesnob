#!/usr/bin/env python3
# /// script
# requires-python = ">=3.12"
# dependencies = ["numpy==2.5.3", "opencv-python-headless==5.0.0.93"]
# ///
"""Generate the paper/sticker SVG variants and native-size PNG exports.

Requires rsvg-convert on PATH, plus Python's numpy and opencv-python-headless.
The transparent SVG is the editable source; all lettering stays as vector paths.
"""

from copy import deepcopy
from pathlib import Path
import subprocess
from tempfile import TemporaryDirectory
import xml.etree.ElementTree as ET

import cv2
import numpy as np


ASSETS = Path(__file__).resolve().parent.parent / "assets"
SVG = "http://www.w3.org/2000/svg"
PAPER = "#fefdfa"
LIGHT_INK = "#e6edf3"
SCALE = 4
BORDER = 12  # SVG viewBox units, matching the original sticker recipe.
ET.register_namespace("", SVG)


def write_svg(root, path):
    ET.indent(root, space="  ")
    ET.ElementTree(root).write(path, encoding="UTF-8", xml_declaration=True)
    with path.open("a") as file:
        file.write("\n")


def trace_sticker(source):
    """Trace the critic using the original 4x raster/dilate/fill workflow."""
    critic = ET.Element(f"{{{SVG}}}svg", {
        key: source.attrib[key] for key in ("width", "height", "viewBox")
    })
    critic.append(deepcopy(source.find(".//*[@id='ink-artwork']")))
    with TemporaryDirectory(prefix="codesnob-sticker-") as scratch:
        svg_path = Path(scratch) / "critic.svg"
        png_path = Path(scratch) / "critic.png"
        write_svg(critic, svg_path)
        subprocess.run([
            "rsvg-convert", "-w", str(int(source.get("width")) * SCALE),
            "-h", str(int(source.get("height")) * SCALE),
            str(svg_path), "-o", str(png_path),
        ], check=True)
        alpha = cv2.imread(str(png_path), cv2.IMREAD_UNCHANGED)[:, :, 3]

    mask = (alpha > 127).astype(np.uint8) * 255
    radius = BORDER * SCALE
    mask = cv2.dilate(mask, cv2.getStructuringElement(
        cv2.MORPH_ELLIPSE, (2 * radius + 1, 2 * radius + 1)
    ))
    # Fill enclosed areas such as the face, shirt and wineglass.
    padded = cv2.copyMakeBorder(mask, 1, 1, 1, 1, cv2.BORDER_CONSTANT, value=0)
    flood = padded.copy()
    cv2.floodFill(
        flood, np.zeros((padded.shape[0] + 2, padded.shape[1] + 2), np.uint8),
        (0, 0), 255,
    )
    silhouette = cv2.bitwise_or(mask, cv2.bitwise_not(flood)[1:-1, 1:-1])
    silhouette = cv2.GaussianBlur(silhouette, (0, 0), SCALE * 0.8)
    silhouette = (silhouette > 127).astype(np.uint8) * 255
    contours, _ = cv2.findContours(
        silhouette, cv2.RETR_EXTERNAL, cv2.CHAIN_APPROX_NONE
    )
    parts = []
    for contour in contours:
        if cv2.contourArea(contour) < (3 * SCALE) ** 2:
            continue
        points = cv2.approxPolyDP(contour, 0.35 * SCALE, True)[:, 0, :] / SCALE
        parts.append("M " + " L ".join(f"{x:.1f} {y:.1f}" for x, y in points) + " Z")
    return " ".join(parts)


def main():
    source = ET.parse(ASSETS / "codesnob-transparent.svg").getroot()
    artwork_index = list(source).index(source.find(".//*[@id='ink-artwork']"))

    paper = deepcopy(source)
    paper.insert(artwork_index, ET.Element(f"{{{SVG}}}rect", {
        "id": "paper", "width": source.get("width"),
        "height": source.get("height"), "fill": PAPER,
    }))
    write_svg(paper, ASSETS / "codesnob.svg")

    dark = deepcopy(source)
    dark.find(".//*[@id='description']").text = (
        "A skeptical seated critic holding a wineglass, a red correction mark, "
        "and the Codesnob wordmark, for dark backgrounds: the critic as a "
        "die-cut paper sticker, the wordmark in light ink. "
        "All artwork and lettering are vector outlines."
    )
    dark.insert(artwork_index, ET.Element(f"{{{SVG}}}path", {
        "id": "sticker", "fill": PAPER, "stroke": "none",
        "d": trace_sticker(source),
    }))
    dark.find(".//*[@id='lettering']").set("fill", LIGHT_INK)
    write_svg(dark, ASSETS / "codesnob-dark.svg")

    for name in ("codesnob", "codesnob-transparent", "codesnob-dark"):
        subprocess.run([
            "rsvg-convert", str(ASSETS / f"{name}.svg"),
            "-o", str(ASSETS / f"{name}.png"),
        ], check=True)
        print(f"Generated {name}.svg and {name}.png")


if __name__ == "__main__":
    main()
