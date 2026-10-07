// Content links are written root-relative without the deploy base ("/guides/rooms/").
// Prefix them with Astro's `base` so the same Markdown works on GitHub Pages and a custom domain.
import { visit } from 'unist-util-visit';

export default function remarkBaseLinks({ base = '/' } = {}) {
  const prefix = base.replace(/\/+$/, '');
  return tree => {
    if (!prefix) return;
    visit(tree, ['link', 'definition'], node => {
      if (node.url.startsWith('/') && !node.url.startsWith('//') && !node.url.startsWith(prefix + '/')) {
        node.url = prefix + node.url;
      }
    });
  };
}
