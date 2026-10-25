// Every command the frontend can call, and what a gateway does with it (PAN-160).
//
// The owner found `configure_session` missing by selecting a model and getting
// `unsupported_command` — the third unserved-command surprise in two days (PAN-124, PAN-125,
// PAN-159), each one found by a person using the app rather than by anything we run. This
// table is what was missing: exactly one state per command, each with a reason a person could
// disagree with. `tests/gateway-commands.test.mjs` checks it against the real call sites and
// the gateway's real command list, in both directions, so a new call site in none of the three
// states fails the build, and a command the gateway starts serving cannot stay mislabelled.
//
// The three states are not a hierarchy of badness:
//   served      — in the gateway's `COMMANDS`, so a remote client can call it today;
//   gated       — a product decision: the surface is hidden and the app says why;
//   unavailable — a gateway cannot answer it today. The reason says which kind: a mechanism
//                 that belongs to this machine, or a surface M3 has simply not taken on.
//
// A command can be served *and* carry a row in `GATED` (see `start_terminal`): that row
// describes an older gateway that serves turns but not terminals. Today's fate is `served`,
// which is what this table records.

export type RemoteState = 'served' | 'gated' | 'unavailable';
export interface RemoteCommand { state: RemoteState; reason: string | null }

/** The reasons, in one place, so the table reads as arguments rather than as labels. */
const REASONS = {
  LOCAL_DISK: "Reads and writes files on the computer running the app; a gateway serves projects and tasks, not this machine's disk.",
  LOCAL_GIT: "Runs git in this computer's checkout; a gateway has its own checkouts and does not act on this one.",
  WORKTREE: "Worktrees are folders in this computer's checkouts; a gateway manages the checkouts it runs turns in, not this machine's.",
  LOCAL_VM: "Local VMs need this Mac's Virtualization framework; a gateway is a Linux process with no hypervisor behind it.",
  CLOUD: "Cloud sandbox control needs account keys stored on this computer; a gateway holds no cloud credentials of its own.",
  SIMULATOR: "iOS and Android simulators are processes on this Mac; a gateway host runs none of them.",
  BROWSER: "The browser panel is a webview inside this app; a gateway has no window to draw in.",
  BOTS: "Bot configuration lives in this app's own database, and the gateway does not expose that surface.",
  MOODBOARDS: "Moodboards are image files in this app's data directory; a gateway's data directory holds no boards.",
  JEV: "Jev runs as a process on this computer and writes its results here; a gateway does not drive it.",
  GITHUB: "Needs the gh CLI and this computer's sign-in, working on this computer's checkouts.",
  THEME: "Themes are files on this computer, and importing one reads a .vsix from its disk.",
  PROVIDER: "Provider credentials and settings live on this computer; a gateway reads its own host's settings and does not accept them from a client.",
  ARCHITECT: "Named agent teams are driven by the window that owns their runs; a gateway does not serve teams.",
  CLI_UPDATE: "Changes an installed CLI in this computer's filesystem; a gateway updates nothing here.",
  INTEGRATIONS: "Agent integrations are detected on this computer, not on the backend.",
  APP_LOCAL: "Describes this window or this installation (appearance, support, metrics, token saver); a backend has no such surface.",
  SLASH: "Slash commands are read from the agent CLI installed on this computer.",
  CODEX: "Drives a local Codex app-server and its job queue on this computer.",
  IMPORT: "Reads other harnesses' session files from this computer's home directory.",
  SUBAGENTS: "Subagent activity is this app's own view of the runs a window started.",
  USAGE_LOCAL: "Usage limits and prompt activity are read from this computer's credentials and logs.",
  NOT_SERVED: "The gateway does not serve this command yet; PAN-160 recorded it here rather than letting the next person find it by tripping over it.",
} as const;

/** Surfaces hidden deliberately, each with the sentence that explains it. */
const GATED: Record<string, string> = {
  bots_list: "PAN-101: bots live in this app's database and the gateway serves none of that surface, so the sidebar section is hidden.",
  browser_open: "PAN-101: the browser panel is a webview in this app; a gateway offers no window, so the toggle is hidden rather than opening nothing.",
  check_agent_updates: "PAN-101: updating an installed CLI changes this computer's filesystem, so the control is hidden rather than failing.",
  get_run_script: "PAN-101: the run script is read from the checkout on this computer; a gateway's checkout is a different one.",
  get_run_script_preview: "PAN-101: the run pad is hidden on a gateway because the script is read from this computer's checkout, so its preview is never asked for either.",
  github_status: "PAN-101: a gateway has no gh CLI and no checkout of its own, so the GitHub view is hidden instead of failing.",
  jev_configuration: "PAN-101: Jev runs on this computer, so its settings are hidden on a gateway.",
  list_agent_integrations: "PAN-101: integrations are detected on this computer, so the settings row is hidden on a gateway.",
  open_support: "PAN-101: the support panel describes this installation, so the menu entry is hidden on a gateway.",
  overlay_toggle: "PAN-101: the quick overlay is this window's own surface; a gateway cannot open one.",
  simulator_status: "PAN-101: simulators are processes on this Mac; the tab is hidden on a gateway.",
  theme_import: "PAN-101: importing a theme reads a .vsix from this computer's disk, and the gateway has no upload route for one.",
  vm_status: "PAN-101: local VMs need this Mac's Virtualization framework; the tab is hidden on a gateway instead of showing an empty panel.",
};

/** What a gateway cannot answer, grouped by the reason that explains each group. */
const UNAVAILABLE: { reason: keyof typeof REASONS; commands: string[] }[] = [
  {
    reason: "LOCAL_DISK",
    commands: [
      "import_prompt_image", "list_files", "list_local_servers", "prepare_file_preview", "read_file",
      "resolve_workspace_files", "save_task_export", "search_files", "workspace_image",
    ],
  },
  {
    reason: "LOCAL_GIT",
    commands: [
      "git_branches", "git_commit", "git_stage", "git_status",
    ],
  },
  {
    reason: "WORKTREE",
    commands: [
      "git_switch_branch", "merge_worktree", "recreate_worktree", "remove_worktree", "worktree_status",
    ],
  },
  {
    reason: "LOCAL_VM",
    commands: [
      "vm_create", "vm_desktop", "vm_frame", "vm_input", "vm_power",
      "vm_setup", "vm_show",
    ],
  },
  {
    reason: "CLOUD",
    commands: [
      "cloud_connect", "cloud_connections", "cloud_create", "cloud_disconnect", "cloud_frame",
      "cloud_input", "cloud_machines", "cloud_power", "cloud_setup_sandboxes",
    ],
  },
  {
    reason: "SIMULATOR",
    commands: [
      "android_setup", "simulator_appearance", "simulator_capture", "simulator_create_duo", "simulator_frame",
      "simulator_input", "simulator_pose", "simulator_power", "simulator_select",
    ],
  },
  {
    reason: "BROWSER",
    commands: [
      "browser_action", "browser_bounds", "browser_capture", "browser_close", "browser_inspect",
      "browser_pick",
    ],
  },
  {
    reason: "BOTS",
    commands: [
      "bot_activity", "bot_channel_add", "bot_channel_configure", "bot_channel_remove", "bot_channel_set_enabled",
      "bot_channel_test", "bot_create", "bot_delete", "bot_desktop_thread", "bot_detail",
      "bot_forget", "bot_message", "bot_pairing_decide", "bot_remember", "bot_reset",
      "bot_rule_add", "bot_rule_delete", "bot_run_schedule_now", "bot_schedule_create", "bot_schedule_delete",
      "bot_schedule_set_enabled", "bot_set_paused", "bot_update", "bot_web_link", "bot_web_new_link",
    ],
  },
  {
    reason: "MOODBOARDS",
    commands: [
      "add_moodboard_image", "create_moodboard", "delete_moodboard", "import_moodboard_images", "list_moodboards",
      "moodboard_image", "read_moodboard", "remove_moodboard_image", "rename_moodboard", "update_moodboard_image",
    ],
  },
  {
    reason: "JEV",
    commands: [
      "jev_assist_list", "jev_assist_prepare", "jev_assist_run", "jev_assist_save_rules", "jev_cancel",
      "jev_doctor", "jev_install_runtime", "jev_list_runs", "jev_read_run", "jev_save_configuration",
      "jev_start", "jev_watch_run",
    ],
  },
  {
    reason: "GITHUB",
    commands: [
      "github_clone_repo", "github_create_pull_request", "github_detect_pull_request", "github_issues", "github_link_pull_request",
      "github_merge_pull_request", "github_project_repos", "github_pull_request", "github_pull_request_diff", "github_refresh_links",
      "github_repo_issues", "github_repo_pulls", "github_repo_runs", "github_repos", "github_resolve",
      "github_search",
    ],
  },
  {
    reason: "THEME",
    commands: [
      "theme_install", "theme_remove", "themes_list",
    ],
  },
  {
    reason: "PROVIDER",
    commands: [
      "list_opencode_models", "provider_check_connection", "provider_claude_status", "provider_configuration", "provider_set_claude_connection_mode",
      "provider_set_executable", "provider_set_mimo_endpoint", "save_opencode_models",
    ],
  },
  {
    reason: "ARCHITECT",
    commands: [
      "architect_configure", "architect_control", "architect_delete_profile", "architect_get", "architect_profiles",
      "architect_save_profile", "architect_team_messages", "architect_worker_messages",
    ],
  },
  {
    reason: "CLI_UPDATE",
    commands: [
      "update_agent",
    ],
  },
  {
    reason: "INTEGRATIONS",
    commands: [
      "list_app_tools",
    ],
  },
  {
    reason: "APP_LOCAL",
    commands: [
      "configure_window_appearance", "get_metrics", "open_external_url", "support_details", "token_saver_install",
      "token_saver_tools",
    ],
  },
  {
    reason: "SLASH",
    commands: [
      "list_slash_commands", "resolve_slash_command",
    ],
  },
  {
    reason: "CODEX",
    commands: [
      "archive_codex_task", "codex_command", "codex_jobs", "fork_codex_task",
    ],
  },
  {
    reason: "IMPORT",
    commands: [
      "import_conversations", "list_importable_conversations",
    ],
  },
  {
    reason: "SUBAGENTS",
    commands: [
      "list_subagents",
    ],
  },
  {
    reason: "USAGE_LOCAL",
    commands: [
      "clear_usage_limit", "prompt_activity",
    ],
  },
  {
    reason: "NOT_SERVED",
    commands: [
      "agent_ui_request_active", "configure_session", "create_board_task", "discard_draft", "export_task",
      "fork_session_at_message", "get_message_content", "get_task", "pin_session", "project_icon",
      "reorder_sessions", "set_reasoning_effort", "set_resume_at_reset", "set_task_link", "set_task_preference",
      "set_task_stage", "task_goal", "turn_change_diff", "turn_change_summary", "update_session",
      "update_task_details",
    ],
  },
];

/**
 * The commands the gateway serves, mirrored from `src-tauri/src/gateway/dispatch.rs`
 * `COMMANDS`. The test compares the two, so this cannot drift silently: a command added to the
 * gateway must be marked here, and one removed must be reclassified.
 */
export const SERVED: string[] = [
  "add_project", "agent_usage", "close_terminal", "create_session",
  "delete_session", "gateway_list_dirs", "gateway_live_tasks", "gateway_status",
  "get_messages", "get_terminal_activity", "get_terminal_preferences", "list_projects",
  "list_provider_models", "list_providers", "list_sessions", "queue_add",
  "queue_list", "queue_remove", "queue_reorder", "queue_restore",
  "queue_take", "queue_update", "remove_project", "resize_terminal",
  "resolve_agent_ui_action", "respond_approval", "respond_user_input", "send_message",
  "start_harness_terminal", "start_task", "start_terminal", "steer_task",
  "stop_session", "usage_ledger", "write_terminal",
];

/** One state per command the frontend can call. */
export const REMOTE_COMMANDS: Record<string, RemoteCommand> = (() => {
  const table: Record<string, RemoteCommand> = {};
  for (const command of SERVED) table[command] = { state: 'served', reason: null };
  for (const [command, reason] of Object.entries(GATED)) table[command] = { state: 'gated', reason };
  for (const group of UNAVAILABLE) for (const command of group.commands) table[command] = { state: 'unavailable', reason: REASONS[group.reason] };
  return table;
})();

/**
 * Call sites that name their command in a variable, with the commands handed to them. The
 * test finds those sites by reading the sources and requires each one here, so a new dynamic
 * dispatch cannot slip past the table the way a literal is counted.
 */
export const OTHER_TRANSPORTS: { transport: string; commands: string[]; why: string }[] = [
  {
    transport: 'callShell',
    commands: ["gateway_config", "gateway_connect", "gateway_disconnect"],
    why: 'Answered by this computer in every mode by construction: the gateway address itself and native dialogs. A gateway cannot be asked to save the address it is reached at, and a native dialog belongs to the window that opened it.',
  },
  {
    transport: 'overlayCall',
    commands: ["fork_session_at_message", "get_message_content", "list_provider_models", "list_providers", "overlay_hide", "overlay_launch", "overlay_open_task", "overlay_preferences", "overlay_recording", "overlay_save_preferences", "overlay_select_model", "overlay_snapshot", "overlay_sync_context", "overlay_thread", "overlay_toggle", "search_files"],
    why: "The launcher window's own IPC (overlay_*), a separate local backend that also names a few commands the main window routes through a gateway. The launcher is local-only, so its transport is not the gateway transport.",
  },
];

/**
 * Call sites that name their command in a variable, with the commands handed to them. The
 * test finds those sites by reading the sources and requires each one here, so a new dynamic
 * dispatch cannot slip past the table the way a literal is counted.
 */
export const DYNAMIC_CALLS: { site: string; commands: string[]; why: string }[] = [
  {
    site: 'src/github-hub.ts',
    commands: ['github_repo_pulls', 'github_repo_issues', 'github_repo_runs'],
    why: 'one fetch helper serves three caches, chosen by which map it is called with',
  },
  {
    site: 'src/jev.ts',
    commands: ['jev_doctor', 'jev_install_runtime'],
    why: 'a typed union, because the doctor and its installer share one code path',
  },
  {
    site: 'src/simulator.ts',
    commands: [],
    why: 'a local wrapper around the host call; every command it passes is a literal in the same file, already counted',
  },
];

/**
 * What a gateway does with `command`, or `undefined` when nobody has said — which the test
 * treats as a failure rather than as "probably fine".
 */
export function remoteFate(command: string): RemoteCommand | undefined {
  return REMOTE_COMMANDS[command];
}
