// Reads an 8-bit RGB or RGBA PNG (what Chromium's captureScreenshot writes) into { width, height, pixel(x, y) }.
// Tests only: pixel assertions without a dependency.
import { readFileSync } from "node:fs";
import { inflateSync } from "node:zlib";

export function readPng(path) {
  const b = readFileSync(path);
  if (b.subarray(1, 4).toString() !== "PNG") throw new Error(`${path} is not a PNG`);
  let width = 0, height = 0, channels = 0, idat = [];
  for (let o = 8; o < b.length;) {
    const len = b.readUInt32BE(o), type = b.toString("latin1", o + 4, o + 8), data = b.subarray(o + 8, o + 8 + len);
    if (type === "IHDR") {
      width = data.readUInt32BE(0); height = data.readUInt32BE(4);
      if (data[8] !== 8 || data[12] !== 0) throw new Error("only 8-bit, non-interlaced PNGs");
      channels = { 2: 3, 6: 4 }[data[9]];
      if (!channels) throw new Error(`PNG color type ${data[9]} not supported`);
    } else if (type === "IDAT") idat.push(data);
    o += 12 + len;
  }
  const raw = inflateSync(Buffer.concat(idat)), stride = width * channels, out = Buffer.alloc(height * stride);
  for (let y = 0; y < height; y++) {
    const f = raw[y * (stride + 1)], line = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
    for (let x = 0; x < stride; x++) {
      const a = x >= channels ? out[y * stride + x - channels] : 0, up = y ? out[(y - 1) * stride + x] : 0;
      const c = x >= channels && y ? out[(y - 1) * stride + x - channels] : 0;
      const p = a + up - c, pa = Math.abs(p - a), pb = Math.abs(p - up), pc = Math.abs(p - c);
      const pred = [0, a, up, (a + up) >> 1, pa <= pb && pa <= pc ? a : pb <= pc ? up : c][f];
      out[y * stride + x] = (line[x] + pred) & 255;
    }
  }
  return { width, height, pixel: (x, y) => { const i = (Math.round(y) * width + Math.round(x)) * channels; return [out[i], out[i + 1], out[i + 2]]; } };
}
