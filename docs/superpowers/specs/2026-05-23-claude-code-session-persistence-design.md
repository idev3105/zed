# Claude Code Session Persistence in Terminal Sidebar

**Date:** 2026-05-23  
**Status:** Approved

## Problem

When a user opens a terminal thread in Zed's agent sidebar and runs `claude`, the Claude Code session starts but Zed has no awareness of the session ID. If the user later wants to continue the same conversation, they must manually look up and pass the session ID. There is no way to click on a past terminal thread and automatically resume it.

## Goal

When a terminal thread in the sidebar is used to run Claude Code, Zed should:
1. Automatically detect and persist the Claude Code session ID into the thread's metadata.
2. Allow the user to click on that thread to auto-resume the session in a new terminal.

## User Flow

### Capture
1. User clicks "New Terminal Thread" in the agent sidebar.
2. User types `claude` and hits Enter.
3. Claude Code starts and creates a session file at `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
4. Zed's `ClaudeSessionWatcher` detects the new file and saves the session ID to the thread's SQLite metadata.
5. The thread entry in the sidebar updates to reflect it has a persisted Claude Code session.

### Resume
1. User clicks on a thread entry that has a saved `claude_session_id`.
2. Zed creates a new terminal thread.
3. Zed writes `claude --resume <session-id>\n` to the terminal's PTY automatically.
4. Claude Code resumes the previous session.

## Architecture

### New component: `ClaudeSessionWatcher`

A watcher spawned once per terminal thread, living in `crates/agent_ui/src/`. It:

- Resolves the Claude Code projects directory: `~/.claude/projects/<base64url(working_directory)>/`
- Uses `fs::watch` to listen for new `.jsonl` files appearing in that directory.
- Only considers files with an `mtime` strictly after `terminal.created_at` (to avoid claiming pre-existing sessions).
- Uses timestamp-based disambiguation when multiple terminals are created in quick succession (see Edge Cases).
- On detection, calls `TerminalThreadMetadataStore::save()` with the updated `claude_session_id`.
- Stops watching once a session ID has been successfully claimed (one session per thread lifecycle).
- Is dropped when the `TerminalThreadMetadata` entry is removed.

### Schema change

Add one nullable column to the existing SQLite table:

```sql
ALTER TABLE sidebar_terminal_threads
ADD COLUMN claude_session_id TEXT;
```

A new DB migration entry is added to `TerminalThreadMetadataDb::MIGRATIONS`.

### `TerminalThreadMetadata` struct change

```rust
pub struct TerminalThreadMetadata {
    // ... existing fields ...
    pub claude_session_id: Option<SharedString>,
}
```

### Resume click handler

In the sidebar's click handler for terminal thread entries, check for `claude_session_id`:

- If `Some(session_id)`: create a new terminal thread and call `terminal.write_to_pty(format!("claude --resume {session_id}\n").into_bytes())` immediately after spawn.
- If `None`: existing behaviour (switch to the terminal, or open a new one).

## Edge Cases

| Situation | Handling |
|---|---|
| `~/.claude/` does not exist (Claude Code not installed) | Watcher directory does not exist → watcher exits silently; thread works normally without a session ID |
| Two terminals created within 2 seconds of each other in the same working directory | Both enter a `Pending` state; after a 2-second settling window, each session file is assigned to the terminal whose `created_at` timestamp is closest to (and before) the file's `mtime` |
| Session file deleted after the fact | Resume proceeds; Claude Code surfaces the error in the terminal |
| Terminal has no `working_directory` | No watcher is spawned; no session ID is captured |
| User manually runs `claude --resume old-id` in a thread that already has a session | New session file detected → `claude_session_id` is overwritten with the new session |
| User starts multiple `claude` processes in the same terminal thread | The first detected session file is claimed; subsequent ones are ignored for that thread |

## Watcher Lifecycle

- **Start:** Immediately after `TerminalThreadMetadata` is saved for a new terminal thread.
- **Stop (success):** After a session ID has been claimed and persisted.
- **Stop (no claude):** If the terminal's process exits without a new session file appearing, the watcher drops with the task.
- **Stop (metadata deleted):** When the thread is removed from `TerminalThreadMetadataStore`, the watcher `Task` is dropped and cancelled automatically.

## Out of Scope

- Support for remote terminals (watcher only runs on local filesystem).
- Integration with any AI agent other than Claude Code.
- Surfacing session history or conversation contents inside Zed.

## Release Notes

- Added automatic Claude Code session ID persistence for terminal threads in the agent sidebar, enabling one-click session resume.
