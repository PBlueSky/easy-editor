// 把 assets/build/ 里的图标像素拼成 Windows 的 .ico。
//
//   node tools/make-ico.mjs <buildDir> <outIco>
//
// .ico = ICONDIR + 若干 ICONDIRENTRY + 各自的图像数据。16..128 用传统 DIB
// （32bpp BGRA + 1bpp AND 掩码，行倒序自下而上），256 直接塞整张 PNG——
// Vista 之后的外壳都认这种条目，省掉一个巨大的位图。
//
// 所有档位都从 1024 那张母图按面积加权缩小，一张都不找 Chrome 单独渲染：
// 无头截图在小窗口上不可靠（128/160 稳定截成全透明的空图，256 会把画面
// 整体往右推、右边裁掉），而缩小这一步的结果是可控的。alpha 预乘后再平均，
// 边缘不会发暗。
//
// 256 这张除了进 .ico，还会写回 <buildDir>/256.png 给 render-icon.ps1
// 拷成 assets/easy-editor-256.png——运行时窗口/任务栏图标用的就是它。
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { deflateSync } from 'node:zlib';

const [buildDir, outPath] = process.argv.slice(2);
if (!buildDir || !outPath) {
  console.error('用法：node make-ico.mjs <buildDir> <outIco>');
  process.exit(2);
}

const MASTER = 1024;
const dibSizes = [16, 24, 32, 48, 64, 128];
const pngSize = 256;

// 面积加权盒式缩小。源是 BGRA（直通 alpha），目标是同样 BGRA。
function resample(src, sw, sh, dw, dh) {
  const out = Buffer.alloc(dw * dh * 4);
  const rx = sw / dw;
  const ry = sh / dh;
  for (let oy = 0; oy < dh; oy++) {
    const fy0 = oy * ry;
    const fy1 = Math.min(sh, (oy + 1) * ry);
    for (let ox = 0; ox < dw; ox++) {
      const fx0 = ox * rx;
      const fx1 = Math.min(sw, (ox + 1) * rx);
      let pb = 0, pg = 0, pr = 0, pa = 0, wsum = 0;
      for (let iy = Math.floor(fy0); iy < Math.ceil(fy1); iy++) {
        const wy = Math.min(fy1, iy + 1) - Math.max(fy0, iy);
        if (wy <= 0) continue;
        for (let ix = Math.floor(fx0); ix < Math.ceil(fx1); ix++) {
          const wx = Math.min(fx1, ix + 1) - Math.max(fx0, ix);
          if (wx <= 0) continue;
          const w = wx * wy;
          const i = (iy * sw + ix) * 4;
          const a = src[i + 3] / 255;
          pb += src[i] * a * w;
          pg += src[i + 1] * a * w;
          pr += src[i + 2] * a * w;
          pa += a * w;
          wsum += w;
        }
      }
      const o = (oy * dw + ox) * 4;
      if (pa > 0) {
        // 预乘均值再摊回直通：pa 已经是 alpha 加权和，除它正好消掉权重。
        out[o] = Math.min(255, Math.round(pb / pa));
        out[o + 1] = Math.min(255, Math.round(pg / pa));
        out[o + 2] = Math.min(255, Math.round(pr / pa));
      }
      out[o + 3] = Math.min(255, Math.round((pa / wsum) * 255));
    }
  }
  return out;
}

function readMaster() {
  const p = join(buildDir, `${MASTER}.bgra`);
  const src = readFileSync(p);
  if (src.length !== MASTER * MASTER * 4) {
    throw new Error(`${MASTER}.bgra 长度 ${src.length}，应为 ${MASTER * MASTER * 4}`);
  }
  return src;
}

const crcTable = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (const b of buf) c = crcTable[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function pngChunk(type, data) {
  const head = Buffer.alloc(4);
  head.writeUInt32BE(data.length, 0);
  const name = Buffer.from(type, 'ascii');
  const tail = Buffer.alloc(4);
  tail.writeUInt32BE(crc32(Buffer.concat([name, data])), 0);
  return Buffer.concat([head, name, data, tail]);
}

// 最小 PNG 编码器：8 位、颜色类型 6（RGBA）、不交叉、每行 filter 0。
// 输入是 GDI 那套直通 BGRA，这里顺手换成 PNG 要的 RGBA。
function encodePng(bgra, size) {
  const rowBytes = size * 4 + 1;
  const raw = Buffer.alloc(size * rowBytes);
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const i = (y * size + x) * 4;
      const o = y * rowBytes + 1 + x * 4;
      raw[o] = bgra[i + 2];
      raw[o + 1] = bgra[i + 1];
      raw[o + 2] = bgra[i];
      raw[o + 3] = bgra[i + 3];
    }
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk('IHDR', ihdr),
    pngChunk('IDAT', deflateSync(raw, { level: 9 })),
    pngChunk('IEND', Buffer.alloc(0)),
  ]);
}

const master = readMaster();
const png256 = encodePng(resample(master, MASTER, MASTER, pngSize, pngSize), pngSize);
// 运行时窗口图标（src/main.rs 里 include_bytes! 的那份）也出自这里，
// 保证 exe 资源图标和任务栏图标是同一张画。
writeFileSync(join(buildDir, `${pngSize}.png`), png256);

// 传统 DIB 条目。alpha 为 0 的圆角要落进 AND 掩码，否则有些外壳组件会把它涂黑。
function dibEntry(size) {
  const src = resample(master, MASTER, MASTER, size, size);
  const stride = Math.ceil(size / 32) * 4; // 1bpp 每行补到 4 字节
  const xor = Buffer.alloc(size * size * 4);
  const and = Buffer.alloc(stride * size);
  for (let y = 0; y < size; y++) {
    const from = y * size * 4; // 自上而下
    const to = (size - 1 - y) * size * 4; // DIB 要自下而上
    src.copy(xor, to, from, from + size * 4);
    for (let x = 0; x < size; x++) {
      if (src[from + x * 4 + 3] < 128) {
        and[(size - 1 - y) * stride + (x >> 3)] |= 1 << (7 - (x & 7));
      }
    }
  }
  const header = Buffer.alloc(40);
  header.writeUInt32LE(40, 0);
  header.writeInt32LE(size, 4);
  header.writeInt32LE(size * 2, 8); // biHeight 是 XOR + AND 两段之和
  header.writeUInt16LE(1, 12);
  header.writeUInt16LE(32, 14);
  header.writeUInt32LE(0, 16); // BI_RGB
  header.writeUInt32LE(xor.length + and.length, 20);
  return Buffer.concat([header, xor, and]);
}

const images = [
  ...dibSizes.map((s) => ({ size: s, data: dibEntry(s) })),
  { size: pngSize, data: png256 },
];

const dirSize = 6 + 16 * images.length;
let offset = dirSize;
const entries = images.map(({ size, data }) => {
  // 256 在目录里写成 0（一个字节放不下）。
  const dim = size >= 256 ? 0 : size;
  const e = Buffer.alloc(16);
  e[0] = dim;
  e[1] = dim;
  e.writeUInt16LE(1, 4); // 平面
  e.writeUInt16LE(32, 6); // 位深
  e.writeUInt32LE(data.length, 8);
  e.writeUInt32LE(offset, 12);
  offset += data.length;
  return e;
});

const header = Buffer.alloc(6);
header.writeUInt16LE(0, 0);
header.writeUInt16LE(1, 2); // 类型：图标
header.writeUInt16LE(images.length, 4);

writeFileSync(outPath, Buffer.concat([header, ...entries, ...images.map((i) => i.data)]));

const total = dirSize + images.reduce((n, i) => n + i.data.length, 0);
console.log(`  ${outPath}: ${images.length} 档 / ${total} bytes`);
