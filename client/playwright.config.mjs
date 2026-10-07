import { defineConfig } from '@playwright/test';

export default defineConfig({
  testDir: 'e2e',
  timeout: 60_000,
  use: {
    baseURL: 'http://localhost:8099',
    // Real local IPs instead of mDNS names, so two headless pages on one machine connect without a resolver.
    launchOptions: { args: ['--disable-features=WebRtcHideLocalIpsWithMdns'] },
  },
  webServer: { command: 'node e2e/serve.mjs 8099', url: 'http://localhost:8099/e2e/page.html', reuseExistingServer: false },
});
