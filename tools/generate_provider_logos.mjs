import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { mkdir, readFile, rm } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const { chromium } = require(process.env.PLAYWRIGHT_MODULE_PATH || 'playwright');
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const sourceDir = path.join(root, 'src', 'icons', 'providers', 'source');
const outputDir = path.join(root, 'src', 'icons', 'providers', 'rendered', 'tiles');
const rawDir = path.join(root, 'tmp', 'provider-tile-supersample');
const downsampleScript = path.join(root, 'tools', 'downsample_provider_tile.py');
const supersample = 8;

// Each compact surface gets a native logical-size class. Every class is
// rendered independently at each supported Windows DPI bucket so GDI never
// has to resize a 28dp detail-popup tile at runtime.
const sizeClasses = [
  {
    suffix: '',
    buckets: [
      { dpi: 96, chip: 28, radius: 7, inset: 1, logo: 19 },
      { dpi: 120, chip: 35, radius: 9, inset: 1, logo: 24 },
      { dpi: 144, chip: 42, radius: 11, inset: 2, logo: 29 },
      { dpi: 168, chip: 49, radius: 12, inset: 2, logo: 33 },
      { dpi: 192, chip: 56, radius: 14, inset: 2, logo: 38 },
    ],
  },
  {
    suffix: '-c20',
    buckets: [
      { dpi: 96, chip: 20, radius: 5, inset: 1, logo: 14 },
      { dpi: 120, chip: 25, radius: 6, inset: 1, logo: 18 },
      { dpi: 144, chip: 30, radius: 8, inset: 2, logo: 21 },
      { dpi: 168, chip: 35, radius: 9, inset: 2, logo: 25 },
      { dpi: 192, chip: 40, radius: 10, inset: 2, logo: 28 },
    ],
  },
  {
    suffix: '-c16',
    buckets: [
      { dpi: 96, chip: 16, radius: 4, inset: 1, logo: 11 },
      { dpi: 120, chip: 20, radius: 5, inset: 1, logo: 14 },
      { dpi: 144, chip: 24, radius: 6, inset: 2, logo: 17 },
      { dpi: 168, chip: 28, radius: 7, inset: 2, logo: 19 },
      { dpi: 192, chip: 32, radius: 8, inset: 2, logo: 22 },
    ],
  },
];

// Render the complete provider tile offline. This keeps the SVG mark and the
// rounded tile on one antialiasing grid instead of combining a PNG with a
// binary GDI rounded region at runtime.
const variants = [
  {
    name: 'claude-dark',
    source: 'claude.svg',
    background: '#30211E',
    border: '#70483D',
  },
  {
    name: 'claude-light',
    source: 'claude.svg',
    background: '#FFF0EA',
    border: '#F1C8BA',
  },
  {
    name: 'openai',
    source: 'openai.svg',
    color: '#000000',
    background: '#F7F7F5',
    border: '#D4D4D0',
  },
  {
    name: 'antigravity-dark',
    source: 'antigravity.svg',
    background: '#172B4A',
    border: '#3C68A4',
  },
  {
    name: 'antigravity-light',
    source: 'antigravity.svg',
    background: '#E8F0FF',
    border: '#BFD3FF',
  },
];

const executablePath = process.env.CHROMIUM_PATH;
if (!executablePath) throw new Error('CHROMIUM_PATH is required');
const pythonPath = process.env.PYTHON_PATH || (process.platform === 'win32' ? 'python' : 'python3');

await rm(rawDir, { recursive: true, force: true });
await mkdir(rawDir, { recursive: true });
await mkdir(outputDir, { recursive: true });

const browser = await chromium.launch({ executablePath, headless: true });
const context = await browser.newContext({
  colorScheme: 'dark',
  deviceScaleFactor: 1,
  viewport: { width: 512, height: 512 },
});
const page = await context.newPage();

try {
  for (const variant of variants) {
    let svg = await readFile(path.join(sourceDir, variant.source), 'utf8');
    svg = svg.replace('width="1em"', 'width="100%"');
    svg = svg.replace('height="1em"', 'height="100%"');

    for (const sizeClass of sizeClasses) {
      for (const bucket of sizeClass.buckets) {
        const rawChip = bucket.chip * supersample;
        const rawRadius = bucket.radius * supersample;
        const rawInset = bucket.inset * supersample;
        const rawInnerRadius = (bucket.radius - bucket.inset) * supersample;
        const rawLogo = bucket.logo * supersample;
        const rawOutput = path.join(
          rawDir,
          `${variant.name}${sizeClass.suffix}-${bucket.dpi}.png`,
        );
        const output = path.join(
          outputDir,
          `${variant.name}${sizeClass.suffix}-${bucket.dpi}.png`,
        );

        await page.setContent(`
        <style>
          html, body { margin: 0; background: transparent; overflow: hidden; }
          #tile {
            position: relative;
            width: ${rawChip}px;
            height: ${rawChip}px;
            overflow: hidden;
            border-radius: ${rawRadius}px;
            background: ${variant.border};
          }
          #surface {
            position: absolute;
            inset: ${rawInset}px;
            border-radius: ${rawInnerRadius}px;
            background: ${variant.background};
          }
          #logo {
            position: absolute;
            left: 50%;
            top: 50%;
            width: ${rawLogo}px;
            height: ${rawLogo}px;
            color: ${variant.color || '#000000'};
            transform: translate(-50%, -50%);
          }
        </style>
        <div id="tile"><div id="surface"></div><div id="logo">${svg}</div></div>
        `);

        const metadata = await page.evaluate(
          ({ expectedChip, expectedLogo }) => {
            const tile = document.querySelector('#tile').getBoundingClientRect();
            const logo = document.querySelector('#logo svg').getBoundingClientRect();
            return {
              tile: [tile.width, tile.height],
              logo: [logo.width, logo.height],
              expectedChip,
              expectedLogo,
            };
          },
          { expectedChip: rawChip, expectedLogo: rawLogo },
        );
        if (
          metadata.tile[0] !== rawChip ||
          metadata.tile[1] !== rawChip ||
          metadata.logo[0] !== rawLogo ||
          metadata.logo[1] !== rawLogo
        ) {
          throw new Error(
            `invalid tile geometry ${rawOutput}: ${JSON.stringify(metadata)}`,
          );
        }

        await page.locator('#tile').screenshot({
          animations: 'disabled',
          omitBackground: true,
          path: rawOutput,
          scale: 'css',
        });

        const downsample = spawnSync(
          pythonPath,
          [downsampleScript, rawOutput, output, String(bucket.chip), String(supersample)],
          { encoding: 'utf8' },
        );
        if (downsample.status !== 0) {
          throw new Error(
            `tile downsample failed for ${output}: ${downsample.stderr || downsample.stdout}`,
          );
        }
        console.log(
          `${path.relative(root, output)} ${bucket.chip}x${bucket.chip} ` +
            `(logo ${bucket.logo}px, ${supersample}x supersample)`,
        );
      }
    }
  }
} finally {
  await browser.close();
  await rm(rawDir, { recursive: true, force: true });
}
