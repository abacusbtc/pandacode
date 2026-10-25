import { defineConfig } from '@playwright/test';

/**
 * `tests/gateway-e2e.spec.ts` is the one spec that boots the real `pandacode-gateway`
 * binary: it needs a built `dist/` and a compiled cargo target, and a cold
 * `cargo run --features gateway` takes every core for minutes. It must not join a plain
 * `npx playwright test`, where it would fail on a clean checkout and starve the three
 * browser workers beside it on a 4-core host.
 *
 * It joins the suite when it is asked for: named on the command line — which both
 * `npm run gateway:test` and `npx playwright test tests/gateway-e2e.spec.ts` do, since a
 * CLI path only filters files this config already matched — or switched on for the whole
 * run with `PANDACODE_GATEWAY_E2E=1`.
 */
const gatewayE2E = Boolean(process.env.PANDACODE_GATEWAY_E2E)
  || process.argv.some(argument => argument.includes('gateway-e2e.spec.ts') || argument.includes('gateway-boot.spec.ts'));

export default defineConfig({
  testDir: './tests',
  testMatch: ['**/run-project.spec.ts', '**/terminal.spec.ts', '**/thread-order.spec.ts', '**/thread-panels.spec.ts', '**/agent-updates.spec.ts', '**/thread-deletion.spec.ts', '**/agent-usage.spec.ts', '**/grok.spec.ts', '**/compact-menus.spec.ts', '**/attachments.spec.ts', '**/sketch.spec.ts', '**/worktrees.spec.ts', '**/github.spec.ts', '**/github-hub.spec.ts', '**/simulator.spec.ts', '**/colors.spec.ts', '**/token-saver.spec.ts', '**/support.spec.ts', '**/jev-live.spec.ts', '**/jev-assist.spec.ts', '**/jev.spec.ts', '**/message-actions.spec.ts', '**/opencode-models.spec.ts', '**/overlay.spec.ts', '**/moodboards.spec.ts', '**/provider-settings.spec.ts', '**/ui.spec.ts', '**/browser.spec.ts', '**/board.spec.ts', '**/streaming.spec.ts', '**/concurrency.spec.ts', '**/subagents.spec.ts', '**/architect.spec.ts', '**/team-profiles.spec.ts', '**/agent-teams.spec.ts', '**/vm.spec.ts', '**/deploy.spec.ts', '**/turn-changes.spec.ts', '**/wallpapers.spec.ts', '**/wallpaper-settings.spec.ts', '**/settings.spec.ts', '**/layout-preferences.spec.ts', '**/agent-input.spec.ts', '**/agent-tools.spec.ts', '**/slash-commands.spec.ts', '**/file-mentions.spec.ts', '**/command-center.spec.ts', '**/goal-workflow.spec.ts', '**/conversation-import.spec.ts', '**/message-queue.spec.ts', '**/usage-limit.spec.ts', '**/bots.spec.ts', '**/bots-web.spec.ts', '**/markdown.spec.ts', '**/document-viewer.spec.ts', '**/generative-ui.spec.ts', '**/gateway.spec.ts', '**/gateway-turns.spec.ts', ...(gatewayE2E ? ['**/gateway-e2e.spec.ts', '**/gateway-boot.spec.ts'] : [])],
  fullyParallel: true,
  workers: Number(process.env.PW_WORKERS ?? 4),
  timeout: 20_000,
  expect: { timeout: 5_000 },
  reporter: 'list',
  outputDir: 'test-results',
  use: {
    baseURL: 'http://127.0.0.1:1420',
    viewport: { width: 1440, height: 940 },
    headless: true,
    launchOptions: {
      // Playwright creates disposable browser contexts; never use a personal profile.
      executablePath: process.env.PANDACODE_TEST_CHROME ?? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    },
    screenshot: 'only-on-failure',
    trace: 'retain-on-failure',
  },
  webServer: {
    command: 'npm run dev',
    url: 'http://127.0.0.1:1420',
    reuseExistingServer: true,
    timeout: 30_000,
  },
});
