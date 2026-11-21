#!/usr/bin/env node
// Newskite app icon generator.
//
// The icon is the innerpulse app icon (ink tile, the six-bar mark unchanged in one solid ember) with
// the mark lifted and slightly reduced, and one row of eleven small square "article segment" ticks
// beneath it in the desktop-family violet: the left seven are solid, the rest dim. Articles arriving
// and assembling into a file. At 16 px the row is dropped and the kit's whole-pixel stock
// icon is used as is; at 24, 32, 48 and 64 px the bars and ticks are laid out on the icon's own pixel grid
// (rendered at exactly that size, like the kit's snapped icons); from 128 px up the vector is rendered.
// Below 48 px the row is six ticks, one under each bar (four filled), so they stay apart.
//
// Usage (from anywhere):
//
//   BRAND_KIT=/path/to/brand-kit \
//   NODE_PATH=/path/to/node_modules \
//   node packaging/icons/make-icons.mjs
//
//   BRAND_KIT  the innerpulse brand kit directory (holds scripts/pulse.mjs). Required.
//              The kit is the studio's own and is not published; the icons it produces
//              are checked in, so building Newskite never needs it.
//   NODE_PATH  a node_modules directory that contains sharp (npm i sharp). sharp is the only dependency.
//
// Outputs (paths relative to the repository root):
//   packaging/icons/net.innerpulse.Newskite.svg                         scalable icon (dark)
//   packaging/icons/hicolor/<n>x<n>/apps/net.innerpulse.Newskite.png    n = 16 24 32 48 64 128 256 512
//   packaging/icons/light/net.innerpulse.Newskite.svg                   light (paper) variant
//   packaging/icons/light/hicolor/<n>x<n>/apps/net.innerpulse.Newskite.png
//   packaging/icons/preview.png                                         contact sheet, dark and light grounds
//   assets/brand/app-icon-256.png                                       window icon embedded by the GUI
//
// The 16 px dark PNG is copied from assets/brand/kit/icon/png/icon-16.png; the light
// ones are rendered from the kit's own snapped light icon (iconSVG({ snap: true, light: true })).

import { createRequire } from 'node:module';
import { copyFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, '../..');
const KIT = process.env.BRAND_KIT;
if (!KIT) {
  console.error('make-icons: set BRAND_KIT to the innerpulse brand kit directory. The kit is not published; the generated icons are checked in.');
  process.exit(1);
}
const { BRAND, MARK, barHeights, barsSVG, iconSVG } = await import(pathToFileURL(join(KIT, 'scripts/pulse.mjs')).href);

// sharp resolves through NODE_PATH, which ESM imports ignore, so load it with require.
const require = createRequire(join(process.cwd(), 'noop.js'));
let sharp;
try { sharp = require('sharp'); } catch {
  try { sharp = createRequire(import.meta.url)('sharp'); } catch {
    console.error('make-icons: sharp not found. npm i sharp somewhere and point NODE_PATH at its node_modules.');
    process.exit(1);
  }
}

const APP_ID = 'net.innerpulse.Newskite';
const SIZES = [16, 24, 32, 48, 64, 128, 256, 512];
const STOCK = new Set([16]); // the kit's whole-pixel stock icon, no segment row (no room at 16 px)
const SNAP = new Set([24, 32, 48, 64]); // whole-pixel layout rendered at exactly that size
const SMALL = 48; // below this, one tick per bar (6, 4 filled): half-pitch ticks would merge

const VIOLET = { dark: '#b38cff', light: '#6a3fd0' }; // desktop & native family accent: ink phosphor, paper step
const THEME = {
  dark: { tile: BRAND.ink, mark: BRAND.pulse, tick: VIOLET.dark, dim: 0.26 },
  light: { tile: BRAND.paper, mark: BRAND.pulseDeep, tick: VIOLET.light, dim: 0.22 },
};

// Layout, in tile units (tile = 200 units, viewBox -100..100), for the vector icon.
const RADIUS = 0.225; // the kit's tile corner, as a fraction of the side
const K = 0.64; // mark scale (the stock icon uses 0.8); keeps clear space at about 1/4 of the mark's width beside it
const TICKS = 11; // segments on half the bar pitch: every other tick sits under a bar
const FILLED = 7; // ~2/3 arrived
const TICK = 7.5; // tick side, about half a bar's width
const GAP = 16; // mark bottom to tick top
const TICK_R = 1.2; // machined corner on the ticks

const f = (v) => +v.toFixed(2);

/** Vertical layout of the vector icon: the mark and the row centered as one group. */
function vectorLayout() {
  const markHalf = (Math.max(...barHeights()) + MARK.stroke / 2) * K;
  const total = 2 * markHalf + GAP + TICK;
  const top = -total / 2;
  return { markY: top + markHalf, tickY: top + 2 * markHalf + GAP };
}

function vectorSVG(theme, size = 512) {
  const t = THEME[theme];
  const { markY, tickY } = vectorLayout();
  const pitch = (MARK.span * 2 * K) / (TICKS - 1);
  const ticks = Array.from({ length: TICKS }, (_, i) => {
    const x = f(-MARK.span * K + pitch * i - TICK / 2);
    const op = i < FILLED ? '' : ` fill-opacity="${t.dim}"`;
    return `<rect x="${x}" y="${f(tickY)}" width="${TICK}" height="${TICK}" rx="${TICK_R}"${op}/>`;
  }).join('');
  const bars = barsSVG(); // the kit's mark, unchanged
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="-100 -100 200 200" width="${size}" height="${size}" role="img" aria-label="Newskite">` +
    `<title>Newskite</title>` +
    `<rect x="-100" y="-100" width="200" height="200" rx="${200 * RADIUS}" fill="${t.tile}"/>` +
    `<g transform="translate(0 ${f(markY)}) scale(${K})" fill="${t.mark}">${bars}</g>` +
    `<g fill="${t.tick}">${ticks}</g></svg>`;
}

/**
 * Whole-pixel layout for 48 and 64 px, after the kit's snappedBars: whole-pixel bar pitch, bars half the pitch
 * wide, whole-pixel heights, square corners; ticks on half the bar pitch, two pixels square, the row aligned to
 * the bars' left edges.
 */
function snappedSVG(theme, size) {
  const t = THEME[theme];
  const heights = barHeights();
  const gaps = heights.length - 1;
  const pitch = Math.round(size * 0.147 * (K / 0.8)); // 48 -> 6, 64 -> 8
  const w = Math.ceil(pitch / 2);
  const unit = pitch / ((MARK.span * 2) / gaps);
  const first = size / 2 - (pitch * gaps) / 2;
  const centre = (c) => (w % 2 ? Math.floor(c) + 0.5 : Math.round(c));
  const halves = heights.map((h) => Math.max(w, Math.round(h * unit + w / 2)));
  const markHalf = Math.max(...halves);
  const tick = 2;
  const gap = Math.round(size / 12); // 48 -> 4, 64 -> 5
  const top = Math.round((size - (2 * markHalf + gap + tick)) / 2);
  const cy = top + markHalf;
  const bars = halves.map((half, j) => {
    const cx = centre(first + pitch * j);
    return `<rect x="${cx - w / 2}" y="${cy - half}" width="${w}" height="${2 * half}"/>`;
  }).join('');
  const left = centre(first) - w / 2;
  const small = size < SMALL;
  const tp = small ? pitch : pitch / 2;
  const count = small ? heights.length : TICKS;
  const filled = small ? 4 : FILLED;
  const ty = cy + markHalf + gap;
  const ticks = Array.from({ length: count }, (_, i) => {
    // with an even bar width the ticks center under the bars; with an odd one they start at each bar's left edge
    const x = w % 2 ? left + tp * i : left + tp * i + (w - tick) / 2;
    const op = i < filled ? '' : ` fill-opacity="${t.dim}"`;
    return `<rect x="${x}" y="${ty}" width="${tick}" height="${tick}"${op}/>`;
  }).join('');
  const r = f(size * RADIUS);
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${size} ${size}" width="${size}" height="${size}">` +
    `<rect width="${size}" height="${size}" rx="${r}" fill="${t.tile}"/>` +
    `<g shape-rendering="crispEdges"><g fill="${t.mark}">${bars}</g><g fill="${t.tick}">${ticks}</g></g></svg>`;
}

const png = (svg, size) => sharp(Buffer.from(svg), { density: 72 * Math.max(1, 512 / size) })
  .resize(size, size).png({ compressionLevel: 9 }).toBuffer();
const exact = (svg) => sharp(Buffer.from(svg), { density: 72 }).png({ compressionLevel: 9 }).toBuffer();

async function iconPNG(theme, size) {
  if (STOCK.has(size)) {
    if (theme === 'dark') return null; // copied from the kit
    return exact(iconSVG({ size, snap: true, light: true }));
  }
  if (SNAP.has(size)) return exact(snappedSVG(theme, size));
  return png(vectorSVG(theme, size), size);
}

const rendered = { dark: {}, light: {} };
for (const theme of ['dark', 'light']) {
  const base = theme === 'dark' ? HERE : join(HERE, 'light');
  mkdirSync(base, { recursive: true });
  writeFileSync(join(base, `${APP_ID}.svg`), `<?xml version="1.0" encoding="UTF-8"?>\n<!-- Newskite app icon (innerpulse). Generated by packaging/icons/make-icons.mjs; edit that, not this. -->\n${vectorSVG(theme, 256)}\n`);
  for (const size of SIZES) {
    const dir = join(base, 'hicolor', `${size}x${size}`, 'apps');
    mkdirSync(dir, { recursive: true });
    const out = join(dir, `${APP_ID}.png`);
    const buf = await iconPNG(theme, size);
    if (buf) writeFileSync(out, buf);
    else copyFileSync(join(REPO, 'assets/brand/kit/icon/png', `icon-${size}.png`), out);
    rendered[theme][size] = out;
  }
}
copyFileSync(rendered.dark[256], join(REPO, 'assets/brand/app-icon-256.png'));

// Contact sheet: each variant at every size, actual pixels, on an ink ground and on a paper ground.
{
  const pad = 32, gapX = 28, labelH = 34, rowH = 512 + labelH;
  const rowW = SIZES.reduce((a, s) => a + s, 0) + gapX * (SIZES.length - 1);
  const panelW = rowW + 2 * pad, panelH = 2 * rowH + 3 * pad;
  const grounds = [{ fill: BRAND.ink, text: '#b0a99f' }, { fill: BRAND.paper, text: '#454b56' }];
  const layers = [];
  let svgText = '';
  grounds.forEach((g, gi) => {
    const ox = gi * panelW;
    svgText += `<rect x="${ox}" y="0" width="${panelW}" height="${panelH}" fill="${g.fill}"/>`;
    ['dark', 'light'].forEach((theme, ti) => {
      const oy = pad + ti * (rowH + pad);
      let x = ox + pad;
      for (const size of SIZES) {
        layers.push({ input: rendered[theme][size], left: x, top: oy + 512 - size });
        svgText += `<text x="${x + size / 2}" y="${oy + 512 + 24}" fill="${g.text}" font-family="JetBrains Mono, DejaVu Sans Mono, monospace" font-size="14" text-anchor="middle">${size}</text>`;
        x += size + gapX;
      }
    });
  });
  const W = panelW * 2, H = panelH;
  const sheet = await sharp(Buffer.from(`<svg xmlns="http://www.w3.org/2000/svg" width="${W}" height="${H}">${svgText}</svg>`))
    .composite(layers).png({ compressionLevel: 9 }).toBuffer();
  writeFileSync(join(HERE, 'preview.png'), sheet);
}

console.log(`make-icons: wrote ${SIZES.length * 2} PNGs, 2 SVGs, preview.png and assets/brand/app-icon-256.png`);
