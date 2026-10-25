// What a connected gateway can and cannot be asked for. Kept apart from the DOM wiring
// in gateway-settings.ts so the rule itself is unit-testable in Node: that file reads
// `window` at import time and cannot be loaded outside a browser.

/** The surfaces a gateway is not expected to serve, and the command that would make each real. */
export const GATED: { command: string; selector: string }[] = [
  { command: 'browser_open', selector: '#browser-toggle, [data-action="browser"]' },
  { command: 'vm_status', selector: '[data-action="tab-vm"]' },
  { command: 'simulator_status', selector: '[data-action="tab-simulator"]' },
  { command: 'overlay_toggle', selector: '#launcher-heading, #launcher-settings' },
  // A gateway has no upload route for a .vsix, so the file picker is offered only where
  // the backend can import one; the Open VSX install by ID stays as it is.
  { command: 'theme_import', selector: '[data-colors="import"]' },
  { command: 'github_status', selector: '[data-action="github-view"]' },
  { command: 'get_run_script', selector: '[data-action="run-project"]' },
  // Usage is read off this machine's own ledger and harness files, which a gateway
  // process has no window for (`agent_usage.rs:524-526`, `usage_ledger.rs:334-336`),
  // so the command is not served and the button must not offer it.
  { command: 'agent_usage', selector: '#agent-usage-button' },
  // A terminal is a PTY on whichever machine runs it; a gateway that does not serve
  // `start_terminal` cannot open one, and a drawer that opens nothing is worse than
  // no drawer.
  { command: 'start_terminal', selector: '#terminal-toggle' },
  // Bots live in the gateway's own database, but this gateway does not serve them yet:
  // boot must not ask for a list it cannot have, and the sidebar section that would show
  // an empty one is hidden rather than left looking broken (PAN-101).
  { command: 'bots_list', selector: '#sidebar-bots, #sidebar-bots-panel' },
  // Settings controls whose command the backend does not serve. Each one can only
  // produce `unsupported_command` for the person, so the control is hidden and the
  // decision is visible here rather than as an error dialog (PAN-101).
  { command: 'check_agent_updates', selector: '[data-action="agent-updates"]' },
  { command: 'list_agent_integrations', selector: '[data-action="agent-integrations"]' },
  { command: 'jev_configuration', selector: '[data-action="jev-settings"]' },
  { command: 'open_support', selector: '[data-action="support"]' },
];

/**
 * Whether `command` may be asked for. `null` is this computer's own backend, which serves
 * everything; an empty list is a gateway whose answer named no commands, which serves
 * nothing. A command absent from the list must not be asked for at all — the gateway
 * answers it with `unsupported_command`, and a surface that cannot work is hidden.
 */
export function serves(commands: string[] | null, command: string): boolean {
  return !commands || commands.includes(command);
}

/**
 * Whether a gateway advertises a *capability* rather than a command. The rule is the
 * same: `null` is this computer's own backend, which has every capability, and a
 * gateway has only what its answer named. A feature is coarser than a command on
 * purpose — `turns` says "this gateway can drive an agent turn", and a client that
 * does not see it must not offer a remote turn at all (PAN-107).
 */
export function featureServed(features: string[] | null, feature: string): boolean {
  return !features || features.includes(feature);
}

/**
 * Whether a backend can drive an agent turn, which is what the composer offers.
 *
 * The `turns` feature is the capability (PAN-107) and `send_message` is the command a
 * send actually uses; a client needs both to be honest about it. A gateway that
 * answers one without the other is not a backend to type a prompt into.
 */
export function turnServed(commands: string[] | null, features: string[] | null): boolean {
  return featureServed(features, 'turns') && serves(commands, 'send_message');
}

/** Whether a backend can open a terminal: the `terminal` feature (PAN-110) and its commands. */
export function terminalServed(commands: string[] | null, features: string[] | null): boolean {
  return featureServed(features, 'terminal') && serves(commands, 'start_terminal');
}
