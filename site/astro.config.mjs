// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import { unified } from '@astrojs/markdown-remark';
import starlightTypeDoc, { typeDocSidebarGroup } from 'starlight-typedoc';
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
      expressiveCode: { themes: ['github-dark-default', 'github-light-default'] },
      plugins: [
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
            gitRevision: 'main',
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
