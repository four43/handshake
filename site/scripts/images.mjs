// Renders the social preview (public/og.png) and the README banner (../docs/banner.png) in the site's theme.
// The network is the hero's (src/components/Hero.astro), frozen mid-flow. Run: npm run images
import { chromium } from 'playwright';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const here = (p) => fileURLToPath(new URL(p, import.meta.url));
// Inline, so the page needs no file access.
const font = (p) => `data:font/woff2;base64,${readFileSync(here(`../node_modules/${p}`)).toString('base64')}`;

const C = {
  ink: '#0a0d0a', node: '#101510', lime: '#a3e635', limeText: '#bef264', limeBright: '#ecfccb',
  violet: '#8b5cf6', violetBright: '#ddd6fe', gray2: '#c2cbbf', gray3: '#8b968a',
};

// Same layout as the hero: a host among seven guests, the signaling server off to the side.
function network() {
  const host = { x: 190, y: 245 };
  const peers = [200, 245, 295, 340, 30, 85, 140].map((deg, i) => {
    const a = (deg * Math.PI) / 180, r = i % 2 ? 150 : 128;
    return { x: host.x + Math.cos(a) * r, y: host.y + Math.sin(a) * r };
  });
  const signal = { x: 360, y: 40 };
  const lerp = (a, b, t) => ({ x: a.x + (b.x - a.x) * t, y: a.y + (b.y - a.y) * t });
  const quad = (a, b, bend) => {
    const len = Math.hypot(b.x - a.x, b.y - a.y);
    const c = { x: (a.x + b.x) / 2 - ((b.y - a.y) / len) * bend, y: (a.y + b.y) / 2 + ((b.x - a.x) / len) * bend };
    return { d: `M${a.x},${a.y} Q${c.x},${c.y} ${b.x},${b.y}`, at: t => lerp(lerp(a, c, t), lerp(c, b, t), t) };
  };
  const dot = (p, color, core, r) =>
    `<circle cx="${p.x}" cy="${p.y}" r="${r * 1.6}" fill="${color}" opacity=".75" filter="url(#blur)"/>` +
    `<circle cx="${p.x}" cy="${p.y}" r="${r}" fill="${core}"/>`;

  let out = '';
  for (const [to, bend, ts] of [[host, 30, [0.35, 0.8]], [peers[2], -24, [0.55]], [peers[3], 18, [0.3]]]) {
    const q = quad(signal, to, bend);
    out += `<path d="${q.d}" fill="none" stroke="${C.violet}" stroke-opacity=".5" stroke-width="1.2" stroke-dasharray="3 5"/>`;
    out += ts.map(t => dot(q.at(t), C.violet, C.violetBright, 1.9)).join('');
  }
  peers.forEach((p, i) => {
    out += `<line x1="${host.x}" y1="${host.y}" x2="${p.x}" y2="${p.y}" stroke="${C.lime}" stroke-opacity=".35" stroke-width="1.5"/>`;
    for (const t of [0.22 + (i % 3) * 0.08, 0.58 + (i % 2) * 0.12]) out += dot(lerp(host, p, t), C.lime, C.limeBright, 2.4);
  });
  out += `<circle cx="${host.x}" cy="${host.y}" r="42" fill="url(#glow)"/>`;
  out += `<circle cx="${host.x}" cy="${host.y}" r="27" fill="none" stroke="${C.lime}" stroke-opacity=".35" stroke-width="1.5"/>`;
  for (const [p, r, w] of [[host, 16, 2.5], ...peers.map(p => [p, 10, 1.5])]) {
    out += `<circle cx="${p.x}" cy="${p.y}" r="${r}" fill="${C.node}" stroke="${C.lime}" stroke-width="${w}"/>`;
    out += `<circle cx="${p.x}" cy="${p.y}" r="${r * 0.38}" fill="${C.lime}"/>`;
  }
  out += `<rect x="${signal.x - 14}" y="${signal.y - 14}" width="28" height="28" rx="5" fill="${C.node}" stroke="${C.violet}" stroke-width="1.5"/>`;
  out += `<path d="M${signal.x - 6},${signal.y - 3} h12 M${signal.x - 6},${signal.y + 3} h12" stroke="${C.violetBright}" stroke-width="1.6" stroke-linecap="round"/>`;
  return `<svg viewBox="20 10 380 400" xmlns="http://www.w3.org/2000/svg">
    <defs>
      <radialGradient id="glow"><stop offset="0" stop-color="${C.lime}" stop-opacity=".5"/><stop offset="1" stop-color="${C.lime}" stop-opacity="0"/></radialGradient>
      <filter id="blur" x="-1" y="-1" width="3" height="3"><feGaussianBlur stdDeviation="1.6"/></filter>
    </defs>${out}</svg>`;
}

const logo = `<svg viewBox="0 0 32 32" fill="none">
  <path d="M8 16h16" stroke="${C.lime}" stroke-width="2" stroke-linecap="round" stroke-dasharray="1 4"/>
  <circle cx="7" cy="16" r="5.5" stroke="${C.lime}" stroke-width="2.2"/><circle cx="25" cy="16" r="5.5" stroke="${C.lime}" stroke-width="2.2"/>
  <circle cx="7" cy="16" r="2" fill="${C.lime}"/><circle cx="25" cy="16" r="2" fill="${C.lime}"/><circle cx="16" cy="16" r="2.2" fill="${C.limeBright}"/>
</svg>`;

const base = `
  @font-face { font-family: 'Space Grotesk'; src: url(${font('@fontsource-variable/space-grotesk/files/space-grotesk-latin-wght-normal.woff2')}); font-weight: 300 700; }
  @font-face { font-family: 'JetBrains Mono'; src: url(${font('@fontsource/jetbrains-mono/files/jetbrains-mono-latin-400-normal.woff2')}); }
  * { box-sizing: border-box; margin: 0; }
  html, body { background: transparent; }
  .card {
    position: relative; overflow: hidden; display: flex; align-items: center; color: #fff;
    font-family: 'Space Grotesk', sans-serif;
    background:
      radial-gradient(38rem 26rem at 76% 46%, rgb(163 230 53 / .16), transparent 70%),
      radial-gradient(circle at 1px 1px, rgb(232 238 230 / .07) 1px, transparent 0) 0 0 / 22px 22px,
      ${C.ink};
  }
  .brand { display: flex; align-items: center; gap: 14px; font-weight: 700; letter-spacing: -.02em; }
  .brand svg { flex: none; }
  .pill {
    display: inline-flex; align-items: center; gap: 10px; padding: 6px 14px; border-radius: 999px;
    border: 1px solid rgb(163 230 53 / .35); background: rgb(163 230 53 / .08); color: ${C.limeText};
    font: 14px 'JetBrains Mono', monospace; letter-spacing: .06em; text-transform: uppercase;
  }
  .pill i { width: 8px; height: 8px; border-radius: 50%; background: ${C.lime}; box-shadow: 0 0 10px ${C.lime}; }
  h1 { font-weight: 700; letter-spacing: -.035em; line-height: 1.02; }
  h1 em { font-style: normal; color: ${C.limeText}; text-shadow: 0 0 40px rgb(163 230 53 / .45); }
  .tag { color: ${C.gray2}; line-height: 1.45; }
  .url { font: 18px 'JetBrains Mono', monospace; color: ${C.gray3}; }
  .url b { color: ${C.limeText}; font-weight: 400; }
  .art { position: absolute; }
`;

const images = [
  {
    out: here('../public/og.png'),
    width: 1200, height: 630, scale: 1,
    html: `<div class="card" style="width:1200px;height:630px;padding:0 72px">
      <div style="width:640px;display:flex;flex-direction:column;gap:26px;position:relative;z-index:1">
        <div class="brand" style="font-size:34px"><span style="width:52px;height:52px;display:block">${logo}</span>Handshake</div>
        <span class="pill" style="align-self:flex-start"><i></i>WebRTC signaling · room registry</span>
        <h1 style="font-size:66px">Get players <em>connected</em>. Then get out of the way.</h1>
        <p class="tag" style="font-size:24px;max-width:600px">Self-hosted rooms, join codes, TURN and reconnects for browser games. Game data goes peer to peer.</p>
        <p class="url">four43.github.io/<b>handshake</b></p>
      </div>
      <div class="art" style="right:40px;top:65px;width:500px;height:500px">${network()}</div>
    </div>`,
  },
  {
    out: here('../../docs/banner.png'),
    width: 1280, height: 400, scale: 2,
    html: `<div class="card" style="width:1280px;height:400px;padding:0 64px;border-radius:24px">
      <div style="width:760px;display:flex;flex-direction:column;gap:22px;position:relative;z-index:1">
        <div class="brand" style="font-size:64px"><span style="width:84px;height:84px;display:block">${logo}</span>Handshake</div>
        <p class="tag" style="font-size:28px;max-width:720px">WebRTC signaling and rooms for browser games. Players meet through a tiny self-hosted server, then play <em style="font-style:normal;color:${C.limeText}">peer to peer</em>.</p>
        <span class="pill" style="align-self:flex-start">Rust server · one-file JS client · TURN</span>
      </div>
      <div class="art" style="right:90px;top:12px;width:361px;height:380px">${network()}</div>
    </div>`,
  },
];

const browser = await chromium.launch();
for (const img of images) {
  const page = await browser.newPage({ viewport: { width: img.width, height: img.height }, deviceScaleFactor: img.scale });
  await page.setContent(`<!doctype html><html><head><style>${base}</style></head><body>${img.html}</body></html>`);
  await page.evaluate(() => document.fonts.ready);
  await page.screenshot({ path: img.out, omitBackground: true });
  console.log('wrote', img.out);
  await page.close();
}
await browser.close();
