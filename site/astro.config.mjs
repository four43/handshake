// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { unified } from '@astrojs/markdown-remark';
import starlightTypeDoc, { typeDocSidebarGroup } from 'starlight-typedoc';
import starlightLlmsTxt from 'starlight-llms-txt';
import remarkBaseLinks from './src/plugins/remark-base-links.mjs';

// GitHub Pages serves the repo at https://four43.github.io/handshake/. For a custom domain,
// build with SITE=https://docs.example.com BASE=/
const site = process.env.SITE ?? 'https://four43.github.io';
const base = process.env.BASE ?? '/handshake';

export default defineConfig({
  site,
  base,
  trailingSlash: 'always',
  markdown: { processor: unified({ remarkPlugins: [[remarkBaseLinks, { base }]] }) },
  integrations: [
    starlight({
      title: 'Handshake',
      description: 'WebRTC signaling and room registry for browser games.',
      logo: { src: './src/assets/logo.svg', replacesTitle: false },
      favicon: '/favicon.svg',
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/four43/handshake' }],
      editLink: { baseUrl: 'https://github.com/four43/handshake/edit/main/site/' },
      customCss: [
        '@fontsource-variable/space-grotesk',
        '@fontsource/jetbrains-mono/400.css',
        '@fontsource/jetbrains-mono/600.css',
        './src/styles/theme.css',
      ],
      components: { Hero: './src/components/Hero.astro' },
      // Point agents that read the HTML at the plain-text docs.
      head: [
        { tag: 'link', attrs: { rel: 'alternate', type: 'text/plain', title: 'llms.txt', href: `${base.replace(/\/$/, '')}/llms.txt` } },
        // Link previews: public/og.png, drawn by `npm run images`.
        ...[
          ['property', 'og:image', `${site}${base.replace(/\/$/, '')}/og.png`],
          ['property', 'og:image:width', '1200'],
          ['property', 'og:image:height', '630'],
          ['property', 'og:image:alt', 'Handshake: get players connected, then get out of the way'],
          ['name', 'twitter:card', 'summary_large_image'],
          ['name', 'twitter:image', `${site}${base.replace(/\/$/, '')}/og.png`],
        ].map(([key, name, content]) => ({ tag: 'meta', attrs: { [key]: name, content } })),
      ],
      expressiveCode: { themes: ['github-dark-default', 'github-light-default'] },
      plugins: [
        // /llms.txt, /llms-full.txt and /llms-small.txt for LLMs and agents (https://llmstxt.org/).
        starlightLlmsTxt({
          projectName: 'Handshake',
          description:
            'Handshake is a small self-hosted WebRTC signaling server (Rust) and room registry for browser games, plus a dependency-free JavaScript client (`handshake.js`). It matches players into rooms by join code, relays connection setup (SDP/ICE) and mints TURN credentials; game data then flows peer to peer, host to each guest, and never touches the server.',
          details: [
            '- Source: https://github.com/four43/handshake. Server image: `ghcr.io/four43/handshake:<tag>` (short commit hash from main, or a git tag).',
            '- Games use the JS client; the WebSocket protocol and HTTP API are for writing other clients or debugging.',
            '- Topology is a star: the host talks to each guest; guests never connect to each other.',
            '- The client API reference is generated from `client/handshake.js`; the protocol and config references from the Rust types in `src/lib.rs`.',
            '',
            'Key pages:',
            '',
            ...[
              ['Quickstart', 'quickstart/', 'run the server, host a room, join it'],
              ['Rooms', 'guides/rooms/', 'codes, keys, listing, host controls, resume'],
              ['Connectivity', 'guides/connectivity/', 'TURN, nearby players, connection type'],
              ['Self-hosting', 'guides/self-hosting/', 'Docker, Caddy, coturn, secrets, trusted proxies'],
              ['JS client API', 'reference/client/', 'Handshake, Room, Peer, HandshakeError'],
              ['WebSocket protocol', 'reference/protocol/', 'every message, field and error code'],
              ['HTTP API', 'reference/http/', '/session, /turn, /ws, /healthz, /metrics'],
              ['Server configuration', 'reference/config/', 'every config.toml key with defaults'],
            ].map(([label, path, what]) => `- [${label}](${site}${base.replace(/\/$/, '')}/${path}): ${what}`),
          ].join('\n'),
          customSelectors: { all: ['.sl-anchor-link', '.hs-kicker'] },
          promote: ['index*', 'quickstart*', 'guides/rooms*'],
          demote: ['reference/client/**'],
          exclude: ['guides/game-networking', 'reference/client/classes/emitter'],
        }),
        starlightTypeDoc({
          entryPoints: ['../client/handshake.js'],
          tsconfig: './tsconfig.typedoc.json',
          output: 'reference/client',
          sidebar: { label: 'JS client', collapsed: false },
          typeDoc: {
            // Room and Peer are not exported, but every game uses them: document them anyway.
            plugin: ['typedoc-plugin-missing-exports'],
            placeInternalsInOwningModule: true,
            excludePrivate: true,
            excludeInternal: true,
            excludeExternals: true,
            hideGenerator: true,
            disableSources: true,
            readme: 'none',
            entryFileName: 'index',
            name: 'JS client',
          },
        }),
      ],
      sidebar: [
        { label: 'Start', items: ['quickstart'] },
        { label: 'Guides', items: [{ autogenerate: { directory: 'guides' } }] },
        {
          label: 'Reference',
          items: ['reference/protocol', 'reference/http', 'reference/config', typeDocSidebarGroup],
        },
      ],
    }),
  ],
});
