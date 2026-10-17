// The Live Activity widget can't draw react-native-svg, so its mascot poses are PNGs in the target's
// asset catalog, written by `npm run mascot`. Each one is decoded here and compared pixel for pixel
// with the pose the app draws, so the widget never shows a stale or different Octet.
import { readFileSync } from "node:fs";
import path from "node:path";
import { inflateSync } from "node:zlib";

import { POSES, type PoseName } from "./poses";

const assets = path.resolve(__dirname, "..", "..", "targets", "octet-live", "Assets.xcassets");

/** The PNG's size and its RGBA pixels, for 8-bit RGBA, non-interlaced images with filter 0. */
function decode(file: string): { width: number; height: number; rgba: Buffer } {
  const png = readFileSync(file);
  expect(png.subarray(0, 8).toString("hex")).toBe("89504e470d0a1a0a");
  let offset = 8;
  let width = 0;
  let height = 0;
  const idat: Buffer[] = [];
  while (offset < png.length) {
    const length = png.readUInt32BE(offset);
    const type = png.subarray(offset + 4, offset + 8).toString("latin1");
    const data = png.subarray(offset + 8, offset + 8 + length);
    if (type === "IHDR") {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      // bit depth 8, colour type 6 (RGBA), compression 0, filter 0, no interlace
      expect([...data.subarray(8, 13)]).toEqual([8, 6, 0, 0, 0]);
    } else if (type === "IDAT") {
      idat.push(data);
    }
    offset += 12 + length;
  }
  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * 4;
  const rgba = Buffer.alloc(stride * height);
  for (let y = 0; y < height; y++) {
    expect(raw[y * (stride + 1)]).toBe(0); // no filter
    raw.copy(rgba, y * stride, y * (stride + 1) + 1, (y + 1) * (stride + 1));
  }
  return { width, height, rgba };
}

/** The pose as the app draws it: each colour's rects, in order, on a transparent ground. */
function rasterize(pose: PoseName): { width: number; height: number; rgba: Buffer } {
  const { viewBox, paths } = POSES[pose];
  const [, , width = 0, height = 0] = viewBox.split(" ").map(Number);
  const rgba = Buffer.alloc(width * height * 4);
  for (const { fill, d } of paths) {
    const [r, g, b] = [1, 3, 5].map((i) => parseInt(fill.slice(i, i + 2), 16));
    for (const m of d.matchAll(/M(\d+) (\d+)h(\d+)v(\d+)h-\d+[zZ]/g)) {
      const [x0, y0, w, h] = [m[1], m[2], m[3], m[4]].map(Number) as [number, number, number, number];
      for (let y = y0; y < y0 + h; y++) {
        for (let x = x0; x < x0 + w; x++) {
          rgba.set([r ?? 0, g ?? 0, b ?? 0, 255], (y * width + x) * 4);
        }
      }
    }
  }
  return { width, height, rgba };
}

describe("the widget's mascot images", () => {
  it.each([
    ["coding", "running"],
    ["approval", "approval"],
    ["success", "done"],
  ] as const)("mascot-%s (for %s) is the app's pose, pixel for pixel", (pose, _kind) => {
    const dir = path.join(assets, `mascot-${pose}.imageset`);
    const contents = JSON.parse(readFileSync(path.join(dir, "Contents.json"), "utf8")) as {
      images: { filename: string; idiom: string; scale?: string }[];
    };
    expect(contents.images).toEqual([{ filename: `mascot-${pose}.png`, idiom: "universal" }]);
    const image = decode(path.join(dir, `mascot-${pose}.png`));
    const expected = rasterize(pose);
    expect([image.width, image.height]).toEqual([expected.width, expected.height]);
    expect(image.rgba.equals(expected.rgba)).toBe(true);
  });

  it("are the only images in the catalog", () => {
    const fs = jest.requireActual<typeof import("fs")>("fs");
    const sets = fs.readdirSync(assets).filter((name) => name.endsWith(".imageset"));
    expect(sets.sort()).toEqual(["mascot-approval.imageset", "mascot-coding.imageset", "mascot-success.imageset"]);
  });
});
