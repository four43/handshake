// Tiny static server for the e2e page: serves client/ so page.html can import ../handshake.js.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const port = Number(process.argv[2] ?? 8099);
const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript' };

createServer(async (req, res) => {
  const path = normalize(join(root, new URL(req.url, 'http://x').pathname));
  if (!path.startsWith(root)) return res.writeHead(403).end();
  try {
    const body = await readFile(path);
    res.writeHead(200, { 'content-type': types[extname(path)] ?? 'application/octet-stream' }).end(body);
  } catch {
    res.writeHead(404).end();
  }
}).listen(port);
