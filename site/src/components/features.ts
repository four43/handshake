// Node/edge layouts for the landing page feature cards (220 x 96 canvas). See Flow.astro.
import type { FlowEdge, FlowNode } from './Flow.astro';

type Art = { nodes: FlowNode[]; edges: FlowEdge[] };

export const rooms: Art = {
  nodes: [
    { id: 'h', x: 70, y: 48, kind: 'host', r: 12 },
    { id: 'a', x: 150, y: 16 }, { id: 'b', x: 172, y: 50 }, { id: 'c', x: 146, y: 82 },
    { id: 'd', x: 18, y: 26, kind: 'ghost', r: 8 },
  ],
  edges: [
    { from: 'h', to: 'a', out: 1, back: 1 }, { from: 'h', to: 'b', out: 2, dur: 1.5 }, { from: 'h', to: 'c', out: 1, back: 1, dur: 2.1 },
    { from: 'd', to: 'h', kind: 'signal', out: 1, dur: 2.4 },
  ],
};

export const p2p: Art = {
  nodes: [
    { id: 's', x: 110, y: 14, kind: 'ghost', r: 7 },
    { id: 'a', x: 28, y: 60 }, { id: 'b', x: 192, y: 60 },
  ],
  edges: [{ from: 'a', to: 'b', out: 3, back: 2, bend: -14, dur: 2.2 }],
};

export const turn: Art = {
  nodes: [{ id: 'a', x: 22, y: 70 }, { id: 'r', x: 110, y: 28, kind: 'relay', r: 12 }, { id: 'b', x: 198, y: 70 }],
  edges: [
    { from: 'a', to: 'r', out: 2, back: 1, dur: 1.6 },
    { from: 'r', to: 'b', out: 2, back: 1, dur: 1.6 },
  ],
};

export const resume: Art = {
  nodes: [{ id: 'g', x: 40, y: 48, kind: 'ghost' }, { id: 'h', x: 180, y: 48, kind: 'host', r: 12 }],
  edges: [
    { from: 'g', to: 'h', out: 1, bend: 26, dur: 2.6 },
    { from: 'h', to: 'g', out: 1, bend: 26, dur: 2.6 },
  ],
};

export const channels: Art = {
  nodes: [{ id: 'h', x: 30, y: 48, kind: 'host', r: 12 }, { id: 'p', x: 190, y: 48 }],
  edges: [
    { from: 'h', to: 'p', out: 5, dur: 1.1, bend: -18 },
    { from: 'h', to: 'p', out: 1, back: 1, dur: 3, bend: 18, kind: 'signal' },
  ],
};

export const tiny: Art = {
  nodes: [
    { id: 's', x: 110, y: 48, kind: 'server', r: 13 },
    { id: 'a', x: 30, y: 20 }, { id: 'b', x: 30, y: 78 }, { id: 'c', x: 190, y: 20 }, { id: 'd', x: 190, y: 78 },
  ],
  edges: ['a', 'b', 'c', 'd'].map((id, i) => ({ from: 's', to: id, kind: 'signal' as const, out: 1, back: 1, dur: 2.6 + i * 0.4 })),
};
