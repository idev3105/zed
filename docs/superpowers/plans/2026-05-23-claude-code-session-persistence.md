# Claude Code Session Persistence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist Claude Code session IDs in terminal thread metadata so that clicking a terminal thread in the agent sidebar automatically resumes the previous Claude Code session with `claude --resume <session-id>`.

**Architecture:** A new `ClaudeSessionWatcher` watches `~/.claude/projects/<encoded-cwd>/` for new `.jsonl` session files created after the terminal starts. When a session file is detected, its stem (the session ID) is saved to SQLite via `TerminalThreadMetadataStore::try_claim_session`. When a terminal thread is restored from the sidebar, if `claude_session_id` is present in its metadata, `insert_terminal` writes `claude --resume <id>\n` to the terminal PTY immediately after creation.

**Tech Stack:** Rust, GPUI async tasks, `fs::Fs::watch()` stream, SQLite via `sqlez`, `agent_ui` crate (Cargo.toml already has `base64`, `chrono`, `futures`, `fs` deps)

---

## File Structure

| File | Action | Responsibility |
|---|---|---|
| `crates/agent_ui/src/terminal_thread_metadata_store.rs` | Modify | Add `claude_session_id` field, DB migration, `try_claim_session` method |
| `crates/agent_ui/src/claude_session_watcher.rs` | Create | `ClaudeSessionWatcher` struct, path encoding helper, async watch loop |
| `crates/agent_ui/src/agent_panel.rs` | Modify | Store watcher in `AgentTerminal`, start watcher in `insert_terminal`, write resume command, thread `claude_session_id` through `spawn_terminal` |
| `crates/agent_ui/src/agent_ui.rs` | Modify | Register `mod claude_session_watcher` |

---

### Task 1: Add `claude_session_id` to `TerminalThreadMetadata` with DB migration

**Files:**
- Modify: `crates/agent_ui/src/terminal_thread_metadata_store.rs`

- [ ] **Step 1: Write the failing test**

Add at the bottom of the `#[cfg(test)] mod tests` block in `terminal_thread_metadata_store.rs`:

```rust
#[gpui::test]
async fn test_claude_session_id_round_trips_through_db(cx: &mut TestAppContext) {
    init_test(cx);

    let store = cx.update(|cx| TerminalThreadMetadataStore::global(cx));
    let terminal_id = TerminalId::new();
    let session_id = SharedString::from("abc-def-1234");
    let paths = WorktreePaths::from_path_lists(
        PathList::new(&[Path::new("/home/user/code")]),
        PathList::new(&[Path::new("/home/user/code")]),
    )
    .unwrap();
    let original = TerminalThreadMetadata {
        terminal_id,
        title: SharedString::from("test"),
        custom_title: None,
        created_at: Utc::now(),
        worktree_paths: paths,
        remote_connection: None,
        working_directory: Some(PathBuf::from("/home/user/code")),
        claude_session_id: Some(session_id.clone()),
    };

    cx.update(|cx| {
        store.update(cx, |store, cx| {
            store.save(original.clone(), cx);
        });
    });
    cx.run_until_parked();

    // Reload from DB
    let reloaded = cx
        .update(|cx| {
            store.read(cx)
                .entry(terminal_id)
                .cloned()
        })
        .unwrap();

    assert_eq!(reloaded.claude_session_id, Some(session_id));
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p agent_ui test_claude_session_id_round_trips_through_db 2>&1 | tail -20
```

Expected: compile error — `claude_session_id` field not found on `TerminalThreadMetadata`.

- [ ] **Step 3: Add `claude_session_id` field to the struct**

In `TerminalThreadMetadata` (line ~48), add the new field:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalThreadMetadata {
    pub terminal_id: TerminalId,
    pub title: SharedString,
    pub custom_title: Option<SharedString>,
    pub created_at: DateTime<Utc>,
    pub worktree_paths: WorktreePaths,
    pub remote_connection: Option<RemoteConnectionOptions>,
    pub working_directory: Option<PathBuf>,
    pub claude_session_id: Option<SharedString>,   // NEW
}
```

- [ ] **Step 4: Add DB migration**

In `TerminalThreadMetadataDb::MIGRATIONS` (line ~378), add a second migration entry:

```rust
const MIGRATIONS: &[&str] = &[
    sql!(
        CREATE TABLE IF NOT EXISTS sidebar_terminal_threads(
            terminal_id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            custom_title TEXT,
            created_at TEXT NOT NULL,
            working_directory TEXT,
            folder_paths TEXT,
            folder_paths_order TEXT,
            main_worktree_paths TEXT,
            main_worktree_paths_order TEXT,
            remote_connection TEXT
        ) STRICT;
    ),
    sql!(
        ALTER TABLE sidebar_terminal_threads
        ADD COLUMN claude_session_id TEXT;
    ),
];
```

- [ ] **Step 5: Update `list()` SELECT query**

In `TerminalThreadMetadataDb::list` (line ~397):

```rust
pub fn list(&self) -> anyhow::Result<Vec<TerminalThreadMetadata>> {
    self.select::<TerminalThreadMetadata>(
        "SELECT terminal_id, title, custom_title, created_at, \
        working_directory, folder_paths, folder_paths_order, main_worktree_paths, \
        main_worktree_paths_order, remote_connection, claude_session_id \
        FROM sidebar_terminal_threads \
        ORDER BY created_at DESC",
    )?()
}
```

- [ ] **Step 6: Update `save()` INSERT statement**

In `TerminalThreadMetadataDb::save` (line ~407), add `claude_session_id` to the upsert:

```rust
pub async fn save(&self, row: TerminalThreadMetadata) -> anyhow::Result<()> {
    let terminal_id = row.terminal_id.to_key_string();
    let title = row.title.to_string();
    let custom_title = row.custom_title.as_ref().map(ToString::to_string);
    let created_at = row.created_at.to_rfc3339();
    let working_directory = row
        .working_directory
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let serialized = row.folder_paths().serialize();
    let (folder_paths, folder_paths_order) = if row.folder_paths().is_empty() {
        (None, None)
    } else {
        (Some(serialized.paths), Some(serialized.order))
    };
    let main_serialized = row.main_worktree_paths().serialize();
    let (main_worktree_paths, main_worktree_paths_order) =
        if row.main_worktree_paths().is_empty() {
            (None, None)
        } else {
            (Some(main_serialized.paths), Some(main_serialized.order))
        };
    let remote_connection = row
        .remote_connection
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .context("serialize terminal thread remote connection")?;
    let claude_session_id = row.claude_session_id.as_ref().map(ToString::to_string);  // NEW

    self.write(move |conn| {
        let sql = "INSERT INTO sidebar_terminal_threads(\
                       terminal_id, title, custom_title, created_at, working_directory, \
                       folder_paths, folder_paths_order, main_worktree_paths, \
                       main_worktree_paths_order, remote_connection, claude_session_id) \
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
                   ON CONFLICT(terminal_id) DO UPDATE SET \
                       title = excluded.title, \
                       custom_title = excluded.custom_title, \
                       created_at = excluded.created_at, \
                       working_directory = excluded.working_directory, \
                       folder_paths = excluded.folder_paths, \
                       folder_paths_order = excluded.folder_paths_order, \
                       main_worktree_paths = excluded.main_worktree_paths, \
                       main_worktree_paths_order = excluded.main_worktree_paths_order, \
                       remote_connection = excluded.remote_connection, \
                       claude_session_id = excluded.claude_session_id";
        let mut stmt = Statement::prepare(conn, sql)?;
        let mut i = stmt.bind(&terminal_id, 1)?;
        i = stmt.bind(&title, i)?;
        i = stmt.bind(&custom_title, i)?;
        i = stmt.bind(&created_at, i)?;
        i = stmt.bind(&working_directory, i)?;
        i = stmt.bind(&folder_paths, i)?;
        i = stmt.bind(&folder_paths_order, i)?;
        i = stmt.bind(&main_worktree_paths, i)?;
        i = stmt.bind(&main_worktree_paths_order, i)?;
        i = stmt.bind(&remote_connection, i)?;
        stmt.bind(&claude_session_id, i)?;
        stmt.exec()
    })
    .await
}
```

- [ ] **Step 7: Update `Column` impl to deserialize the new field**

In the `Column for TerminalThreadMetadata` impl (line ~479), add the new column read after `remote_connection_json`:

```rust
impl Column for TerminalThreadMetadata {
    fn column(statement: &mut Statement, start_index: i32) -> anyhow::Result<(Self, i32)> {
        let (terminal_id, next): (String, i32) = Column::column(statement, start_index)?;
        let (title, next): (String, i32) = Column::column(statement, next)?;
        let (custom_title, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (created_at, next): (String, i32) = Column::column(statement, next)?;
        let (working_directory, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (folder_paths_str, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (folder_paths_order_str, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (main_worktree_paths_str, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (main_worktree_paths_order_str, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (remote_connection_json, next): (Option<String>, i32) = Column::column(statement, next)?;
        let (claude_session_id_str, next): (Option<String>, i32) = Column::column(statement, next)?;  // NEW

        // ... existing path deserialization code unchanged ...

        Ok((
            TerminalThreadMetadata {
                terminal_id: TerminalId::from_key_string(&terminal_id)?,
                title: SharedString::from(title),
                custom_title: custom_title
                    .filter(|title| !title.trim().is_empty())
                    .map(SharedString::from),
                created_at: DateTime::parse_from_rfc3339(&created_at)?.with_timezone(&Utc),
                worktree_paths,
                remote_connection,
                working_directory: working_directory.map(PathBuf::from),
                claude_session_id: claude_session_id_str  // NEW
                    .filter(|s| !s.trim().is_empty())
                    .map(SharedString::from),
            },
            next,
        ))
    }
}
```

- [ ] **Step 8: Fix all struct literal construction sites that don't include the new field**

Run:
```bash
cargo build -p agent_ui 2>&1 | grep "missing field"
```

For each site: add `claude_session_id: None` (e.g., in `terminal_metadata()` in `agent_panel.rs`, and the `metadata()` helper in tests).

- [ ] **Step 9: Run the round-trip test**

```bash
cargo test -p agent_ui test_claude_session_id_round_trips_through_db 2>&1 | tail -10
```

Expected: `test ... ok`

- [ ] **Step 10: Commit**

```bash
git add crates/agent_ui/src/terminal_thread_metadata_store.rs
git commit -m "agent_ui: Add claude_session_id field to TerminalThreadMetadata with DB migration"
```

---

### Task 2: Add `try_claim_session` to `TerminalThreadMetadataStore`

**Files:**
- Modify: `crates/agent_ui/src/terminal_thread_metadata_store.rs`

- [ ] **Step 1: Write the failing tests**

In the `#[cfg(test)] mod tests` block:

```rust
#[gpui::test]
async fn test_try_claim_session_succeeds_for_single_terminal(cx: &mut TestAppContext) {
    init_test(cx);
    let store = cx.update(|cx| TerminalThreadMetadataStore::global(cx));

    let paths = WorktreePaths::default();
    let terminal_id = TerminalId::new();
    let created_at = Utc::now() - chrono::Duration::seconds(5);
    let meta = TerminalThreadMetadata {
        terminal_id,
        title: SharedString::from("test"),
        custom_title: None,
        created_at,
        worktree_paths: paths,
        remote_connection: None,
        working_directory: Some(PathBuf::from("/home/user/code")),
        claude_session_id: None,
    };
    cx.update(|cx| {
        store.update(cx, |store, cx| store.save(meta, cx));
    });

    let session_id = SharedString::from("session-abc");
    let claimed = cx.update(|cx| {
        store.update(cx, |store, cx| {
            store.try_claim_session(terminal_id, created_at, session_id.clone(), cx)
        })
    });
    assert!(claimed);

    let saved_id = cx.update(|cx| {
        store
            .read(cx)
            .entry(terminal_id)
            .and_then(|m| m.claude_session_id.clone())
    });
    assert_eq!(saved_id, Some(session_id));
}

#[gpui::test]
async fn test_try_claim_session_rejects_ambiguous_terminals(cx: &mut TestAppContext) {
    init_test(cx);
    let store = cx.update(|cx| TerminalThreadMetadataStore::global(cx));

    let now = Utc::now();
    let working_dir = Some(PathBuf::from("/home/user/code"));
    let id1 = TerminalId::new();
    let id2 = TerminalId::new();

    for (id, offset_ms) in [(id1, 0i64), (id2, 500i64)] {
        let created_at = now + chrono::Duration::milliseconds(offset_ms);
        let meta = TerminalThreadMetadata {
            terminal_id: id,
            title: SharedString::from("test"),
            custom_title: None,
            created_at,
            worktree_paths: WorktreePaths::default(),
            remote_connection: None,
            working_directory: working_dir.clone(),
            claude_session_id: None,
        };
        cx.update(|cx| {
            store.update(cx, |store, cx| store.save(meta, cx));
        });
    }

    // Should NOT claim: another terminal with same working_dir was created within 2s
    let claimed = cx.update(|cx| {
        store.update(cx, |store, cx| {
            store.try_claim_session(id1, now, SharedString::from("session-xyz"), cx)
        })
    });
    assert!(!claimed);
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p agent_ui test_try_claim_session 2>&1 | tail -10
```

Expected: compile error — `try_claim_session` method not found.

- [ ] **Step 3: Implement `try_claim_session`**

Add to the `impl TerminalThreadMetadataStore` block (after the `save` method):

```rust
/// Attempts to assign `session_id` to the terminal identified by `terminal_id`.
/// Returns `false` if another terminal with the same working directory was created
/// within 2 seconds of `terminal_created_at`, indicating ambiguity.
pub fn try_claim_session(
    &mut self,
    terminal_id: TerminalId,
    terminal_created_at: DateTime<Utc>,
    session_id: SharedString,
    cx: &mut Context<Self>,
) -> bool {
    let Some(metadata) = self.terminals.get(&terminal_id).cloned() else {
        return false;
    };

    let ambiguity_window = chrono::Duration::seconds(2);
    let is_ambiguous = self.terminals.values().any(|other| {
        other.terminal_id != terminal_id
            && other.working_directory == metadata.working_directory
            && (other.created_at - terminal_created_at).abs() < ambiguity_window
    });
    if is_ambiguous {
        return false;
    }

    let mut updated = metadata;
    updated.claude_session_id = Some(session_id);
    self.save(updated, cx);
    true
}
```

- [ ] **Step 4: Run the tests**

```bash
cargo test -p agent_ui test_try_claim_session 2>&1 | tail -10
```

Expected: both tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/agent_ui/src/terminal_thread_metadata_store.rs
git commit -m "agent_ui: Add try_claim_session to TerminalThreadMetadataStore"
```

---

### Task 3: Create `ClaudeSessionWatcher`

**Files:**
- Create: `crates/agent_ui/src/claude_session_watcher.rs`

- [ ] **Step 1: Write the failing tests first (at the bottom of the new file)**

Create `crates/agent_ui/src/claude_session_watcher.rs` with tests only:

```rust
use std::{path::PathBuf, sync::Arc, time::Duration};

use chrono::Utc;
use fs::FakeFs;
use gpui::{App, AsyncApp, Context, Task, WeakEntity};
use ui::SharedString;

use crate::{TerminalId, terminal_thread_metadata_store::TerminalThreadMetadataStore};

/// Wraps the watcher `Task<()>`; dropping this cancels the watcher.
pub struct ClaudeSessionWatcher {
    pub _task: Task<()>,
}

/// Returns the path Claude Code uses to store sessions for the given working directory.
/// Claude Code names the project directory by replacing '/' in the absolute path with '-'.
/// Example: /Users/john/code -> -Users-john-code
pub fn claude_session_dir_for(working_dir: &std::path::Path) -> Option<PathBuf> {
    let home = std::env::home_dir()?;
    let encoded = working_dir.to_str()?.replace('/', "-");
    Some(home.join(".claude").join("projects").join(encoded))
}

/// The watch loop run as a GPUI foreground task.
/// Callers construct the task with `cx.spawn(...)` and store it in `ClaudeSessionWatcher { _task }`.
/// This function does NOT take `Context<AgentPanel>` to avoid circular module dependencies.
pub async fn watch_loop(
        terminal_id: TerminalId,
        created_at: chrono::DateTime<Utc>,
        watch_dir: PathBuf,
        fs: Arc<dyn fs::Fs>,
        store: WeakEntity<TerminalThreadMetadataStore>,
        mut cx: AsyncApp,
    ) {
        // Wait for the watch dir to exist (Claude Code may not have run yet).
        // Poll for up to 30 seconds.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let dir_exists = fs
                .metadata(&watch_dir)
                .await
                .ok()
                .flatten()
                .map_or(false, |m| m.is_dir);
            if dir_exists {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            cx.background_executor()
                .timer(Duration::from_secs(2))
                .await;
        }

        let (mut events, _watcher) = fs.watch(&watch_dir, Duration::from_millis(500)).await;

        use futures::StreamExt as _;
        while let Some(batch) = events.next().await {
            for event in batch {
                if event.kind != Some(fs::PathEventKind::Created) {
                    continue;
                }
                if event.path.extension().map_or(true, |ext| ext != "jsonl") {
                    continue;
                }
                let Some(session_id) = event
                    .path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| SharedString::from(s.to_string()))
                else {
                    continue;
                };

                // Check file mtime is after terminal creation
                let Ok(Some(metadata)) = fs.metadata(&event.path).await else {
                    continue;
                };
                let file_time: std::time::SystemTime = metadata.mtime.into();
                let file_dt = chrono::DateTime::<Utc>::from(file_time);
                if file_dt <= created_at {
                    continue;
                }

                let claimed = store
                    .update(&mut cx, |store, cx| {
                        store.try_claim_session(terminal_id, created_at, session_id, cx)
                    })
                    .unwrap_or(false);

                if claimed {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_thread_metadata_store::{TerminalThreadMetadata, TestTerminalMetadataDbName};
    use crate::thread_metadata_store::WorktreePaths;
    use fs::{FakeFs, PathEvent};
    use gpui::TestAppContext;
    use workspace::PathList;
    use std::path::Path;

    fn make_metadata(terminal_id: TerminalId, working_dir: &str) -> TerminalThreadMetadata {
        TerminalThreadMetadata {
            terminal_id,
            title: SharedString::from("test"),
            custom_title: None,
            created_at: Utc::now() - chrono::Duration::seconds(10),
            worktree_paths: WorktreePaths::default(),
            remote_connection: None,
            working_directory: Some(PathBuf::from(working_dir)),
            claude_session_id: None,
        }
    }

    #[gpui::test]
    async fn test_watcher_claims_new_session_file(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::terminal_thread_metadata_store::TerminalThreadMetadataStore::init_global(cx)
        });

        let store = cx.update(|cx| TerminalThreadMetadataStore::global(cx));
        let terminal_id = TerminalId::new();
        let working_dir = PathBuf::from("/home/user/myproject");
        let meta = make_metadata(terminal_id, "/home/user/myproject");
        let created_at = meta.created_at;

        cx.update(|cx| store.update(cx, |s, cx| s.save(meta, cx)));

        let fake_fs = FakeFs::new(cx.background_executor().clone());
        let watch_dir = PathBuf::from("/fake-home/.claude/projects/-home-user-myproject");
        fake_fs.create_dir(&watch_dir).await.unwrap();

        // We don't have a real AgentPanel to create the watcher from Context<AgentPanel>,
        // so we test watch_loop directly by simulating the fs events.
        // The watch_loop is public for testability.
        let fs: Arc<dyn fs::Fs> = Arc::new(fake_fs.clone());
        let store_weak = store.downgrade();

        let _watcher_task = cx.spawn(async move |mut cx| {
            crate::claude_session_watcher::watch_loop(
                terminal_id,
                created_at,
                watch_dir.clone(),
                fs,
                store_weak,
                cx,
            )
            .await
        });

        cx.run_until_parked();

        // Simulate Claude Code creating a session file
        fake_fs
            .insert_file(
                "/fake-home/.claude/projects/-home-user-myproject/abc1234.jsonl",
                "{}".into(),
            )
            .await;

        cx.run_until_parked();

        let saved_id = cx.update(|cx| {
            store
                .read(cx)
                .entry(terminal_id)
                .and_then(|m| m.claude_session_id.clone())
        });
        assert_eq!(saved_id, Some(SharedString::from("abc1234")));
    }

    #[gpui::test]
    async fn test_watcher_ignores_preexisting_files(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::terminal_thread_metadata_store::TerminalThreadMetadataStore::init_global(cx)
        });

        let store = cx.update(|cx| TerminalThreadMetadataStore::global(cx));
        let terminal_id = TerminalId::new();
        let meta = make_metadata(terminal_id, "/home/user/project");
        let created_at = meta.created_at;
        cx.update(|cx| store.update(cx, |s, cx| s.save(meta, cx)));

        let fake_fs = FakeFs::new(cx.background_executor().clone());
        let watch_dir = PathBuf::from("/fake-home/.claude/projects/-home-user-project");
        fake_fs.create_dir(&watch_dir).await.unwrap();

        // Insert a file BEFORE the terminal started (simulating a preexisting session)
        // FakeFs mtime is controlled by its internal clock; we can't directly set mtime.
        // Instead, we rely on the Created event not being fired for existing files.
        // The watcher only picks up Created events after watch() is called.
        fake_fs
            .insert_file(
                "/fake-home/.claude/projects/-home-user-project/old-session.jsonl",
                "{}".into(),
            )
            .await;

        let fs: Arc<dyn fs::Fs> = Arc::new(fake_fs.clone());
        let store_weak = store.downgrade();

        let _watcher_task = cx.spawn(async move |mut cx| {
            crate::claude_session_watcher::watch_loop(
                terminal_id,
                created_at,
                watch_dir,
                fs,
                store_weak,
                cx,
            )
            .await
        });

        cx.run_until_parked();

        // No new file created after watcher started — nothing should be claimed
        let saved_id = cx.update(|cx| {
            store
                .read(cx)
                .entry(terminal_id)
                .and_then(|m| m.claude_session_id.clone())
        });
        assert_eq!(saved_id, None);
    }
}
```

- [ ] **Step 2: Run to verify compile failure**

```bash
cargo test -p agent_ui test_watcher 2>&1 | head -30
```

Expected: compile errors — struct / methods not yet implemented.

- [ ] **Step 3: Ensure the file compiles with the stubs** — the file above already contains the full implementation including tests. Resolve any import issues:

Check `crates/agent_ui/Cargo.toml` for `fs` dependency path (it is `fs.workspace = true`). Add any missing items to imports. The `watch_loop` method is `pub(crate)` but for tests it needs `pub` — change it to `pub` in the implementation above.

- [ ] **Step 4: Register the new module in `agent_ui.rs`**

In `crates/agent_ui/src/agent_ui.rs`, add after the `terminal_codegen` line:

```rust
pub(crate) mod claude_session_watcher;
```

- [ ] **Step 5: Build to check for compile errors**

```bash
cargo build -p agent_ui 2>&1 | grep "^error" | head -20
```

Fix any import or visibility issues.

- [ ] **Step 6: Run watcher tests**

```bash
cargo test -p agent_ui test_watcher 2>&1 | tail -15
```

Expected: both tests pass.

- [ ] **Step 7: Verify the `claude_session_dir_for` encoding**

Run a quick manual check. In your shell, look at what `~/.claude/projects/` contains and confirm the directory naming matches the output of:

```bash
echo "/your/working/directory" | sed 's|/|-|g'
```

If the encoding differs from the dash-replacement scheme, update `claude_session_dir_for` accordingly and re-run the tests.

- [ ] **Step 8: Commit**

```bash
git add crates/agent_ui/src/claude_session_watcher.rs crates/agent_ui/src/agent_ui.rs
git commit -m "agent_ui: Add ClaudeSessionWatcher for detecting Claude Code sessions"
```

---

### Task 4: Add `_claude_session_watcher` to `AgentTerminal` and start watcher in `insert_terminal`

**Files:**
- Modify: `crates/agent_ui/src/agent_panel.rs`

- [ ] **Step 1: Add watcher field to `AgentTerminal` struct (line ~753)**

```rust
struct AgentTerminal {
    view: Entity<TerminalView>,
    title_editor: Option<Entity<Editor>>,
    title_editor_initial_title: Option<String>,
    title_editor_subscription: Option<Subscription>,
    last_known_title: String,
    working_directory: Option<PathBuf>,
    created_at: DateTime<Utc>,
    has_notification: bool,
    notification_windows: Vec<WindowHandle<AgentNotification>>,
    notification_subscriptions: Vec<Subscription>,
    _subscriptions: Vec<Subscription>,
    _claude_session_watcher: Option<crate::claude_session_watcher::ClaudeSessionWatcher>,  // NEW
}
```

- [ ] **Step 2: Initialize the new field to `None` in `insert_terminal`**

At the line where `AgentTerminal` is constructed (~line 1840), add `_claude_session_watcher: None`:

```rust
let mut terminal = AgentTerminal {
    view: terminal_view,
    title_editor: None,
    title_editor_initial_title: None,
    title_editor_subscription: None,
    last_known_title: initial_title
        .map(|title| title.to_string())
        .unwrap_or_default(),
    working_directory,
    created_at: created_at.unwrap_or_else(Utc::now),
    has_notification: false,
    notification_windows: Vec::new(),
    notification_subscriptions: Vec::new(),
    _subscriptions: vec![view_subscription, terminal_subscription],
    _claude_session_watcher: None,  // NEW — filled in below for local terminals
};
```

- [ ] **Step 3: Build to verify struct construction sites compile**

```bash
cargo build -p agent_ui 2>&1 | grep "^error" | head -10
```

Fix any other struct literal sites that need `_claude_session_watcher: None`.

- [ ] **Step 4: Start the watcher for local terminals at the end of `insert_terminal`**

Immediately after `terminal.refresh_metadata(cx);` (around line 1858), before `self.terminals.insert(...)`, add:

```rust
#[cfg(not(test))]
{
    if !self.project.read(cx).is_remote()
        && let Some(working_dir) = &terminal.working_directory
    {
        use crate::claude_session_watcher::{ClaudeSessionWatcher, claude_session_dir_for, watch_loop};
        if let Some(watch_dir) = claude_session_dir_for(working_dir) {
            let fs = self.project.read(cx).fs().clone();
            if let Some(store) = TerminalThreadMetadataStore::try_global(cx)
                .map(|s| s.downgrade())
            {
                let created_at = terminal.created_at;
                let task = cx.spawn(async move |_this, cx| {
                    watch_loop(terminal_id, created_at, watch_dir, fs, store, cx).await
                });
                terminal._claude_session_watcher = Some(ClaudeSessionWatcher { _task: task });
            }
        }
    }
}
```

- [ ] **Step 5: Build**

```bash
cargo build -p agent_ui 2>&1 | grep "^error" | head -10
```

- [ ] **Step 6: Commit**

```bash
git add crates/agent_ui/src/agent_panel.rs
git commit -m "agent_ui: Start ClaudeSessionWatcher when a new terminal thread is created"
```

---

### Task 5: Write `claude --resume` to PTY when restoring a terminal with a session ID

**Files:**
- Modify: `crates/agent_ui/src/agent_panel.rs`

- [ ] **Step 1: Write the failing test**

In the `#[cfg(test)]` section of `agent_panel.rs` (find the existing terminal restore tests), add:

```rust
#[gpui::test]
async fn test_restore_terminal_writes_claude_resume_to_pty(cx: &mut TestAppContext) {
    // Use the existing test infrastructure in test_support.rs
    let mut cx = crate::test_support::AgentPanelTestContext::new(cx).await;

    let terminal_id = TerminalId::new();
    let session_id = SharedString::from("test-session-abc");
    let metadata = TerminalThreadMetadata {
        terminal_id,
        title: SharedString::from("test"),
        custom_title: None,
        created_at: Utc::now(),
        worktree_paths: cx.worktree_paths(),
        remote_connection: None,
        working_directory: Some(cx.working_directory()),
        claude_session_id: Some(session_id.clone()),
    };

    cx.panel.update(cx.cx, |panel, cx| {
        panel
            .restore_test_terminal(
                metadata,
                false,
                AgentThreadSource::Sidebar,
                None,
                cx.window_cx(),
                cx,
            )
            .unwrap();
    });

    cx.run_until_parked();

    // Access the underlying Terminal entity via the private `terminals` field.
    // AgentTerminal.view is Entity<TerminalView>; TerminalView::terminal() returns Entity<Terminal>.
    let input_log = cx.panel.update(cx.cx, |panel, cx| {
        panel
            .terminals
            .get(&terminal_id)
            .map(|agent_terminal| agent_terminal.view.read(cx).terminal().clone())
            .map(|terminal| terminal.update(cx, |t, _| t.take_input_log()))
    });

    let expected = format!("claude --resume {}\n", session_id).into_bytes();
    let log = input_log.flatten().unwrap_or_default();
    assert!(
        log.contains(&expected),
        "Expected PTY to receive 'claude --resume {session_id}\\n', got: {:?}",
        log
    );
}

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p agent_ui test_restore_terminal_writes_claude_resume 2>&1 | tail -15
```

Expected: compile error or test failure.

- [ ] **Step 3: Add `claude_session_id` parameter to `spawn_terminal`**

Change the signature of `spawn_terminal` (line ~1727):

```rust
fn spawn_terminal(
    &mut self,
    terminal_id: TerminalId,
    working_directory: Option<PathBuf>,
    custom_title: Option<SharedString>,
    initial_title: Option<SharedString>,
    created_at: Option<DateTime<Utc>>,
    select: bool,
    focus: bool,
    source: AgentThreadSource,
    claude_session_id: Option<SharedString>,  // NEW
    window: &mut Window,
    cx: &mut Context<Self>,
)
```

Pass `claude_session_id` through to the `insert_terminal` call inside `spawn_terminal` (line ~1770):

```rust
this.insert_terminal(
    terminal_id,
    terminal_view,
    terminal_working_directory,
    custom_title,
    initial_title,
    created_at,
    select,
    focus,
    source,
    claude_session_id,  // NEW
    window,
    cx,
);
```

- [ ] **Step 4: Update `insert_terminal` signature**

```rust
fn insert_terminal(
    &mut self,
    terminal_id: TerminalId,
    terminal_view: Entity<TerminalView>,
    working_directory: Option<PathBuf>,
    custom_title: Option<SharedString>,
    initial_title: Option<SharedString>,
    created_at: Option<DateTime<Utc>>,
    select: bool,
    focus: bool,
    source: AgentThreadSource,
    claude_session_id: Option<SharedString>,  // NEW
    window: &mut Window,
    cx: &mut Context<Self>,
)
```

At the end of `insert_terminal`, after `self.terminals.insert(terminal_id, terminal)`, add:

```rust
if let Some(session_id) = claude_session_id {
    let terminal_entity = terminal_view.read(cx).terminal().clone();
    terminal_entity.update(cx, |terminal, _cx| {
        terminal.input(
            std::borrow::Cow::Owned(
                format!("claude --resume {}\n", session_id).into_bytes(),
            ),
        );
    });
}
```

- [ ] **Step 5: Update the three `spawn_terminal` call sites to pass `None`**

The three calls are at lines ~1675, ~2034, ~4415. All three pass `None` for the new `claude_session_id` parameter by default. The `restore_terminal` call at ~2034 will be updated next.

For `new_terminal` (line ~1675):
```rust
self.spawn_terminal(
    TerminalId::new(),
    working_directory,
    None,
    None,
    None,
    true,
    true,
    source,
    None,     // claude_session_id
    window,
    cx,
);
```

For `spawn_initial_terminal` (line ~4415):
```rust
self.spawn_terminal(
    terminal_id,
    working_directory,
    None,
    None,
    None,
    true,
    false,
    source,
    None,     // claude_session_id
    window,
    cx,
);
```

- [ ] **Step 6: Update `restore_terminal` to pass the session ID**

In `restore_terminal` (line ~2034), change the `spawn_terminal` call to pass `metadata.claude_session_id.clone()`:

```rust
self.spawn_terminal(
    metadata.terminal_id,
    working_directory,
    metadata.custom_title.clone(),
    initial_title,
    Some(metadata.created_at),
    true,
    focus,
    source,
    metadata.claude_session_id.clone(),  // NEW
    window,
    cx,
);
```

- [ ] **Step 7: Update `insert_display_only_terminal` (test path)**

Add `claude_session_id: Option<SharedString>` to `insert_display_only_terminal` signature and thread it through to the same PTY-write logic at the end of that function (same pattern as in `insert_terminal`).

Update `restore_test_terminal` (line ~6004) to also pass `metadata.claude_session_id.clone()`.

- [ ] **Step 8: Build to check all call sites compile**

```bash
cargo build -p agent_ui 2>&1 | grep "^error" | head -20
```

Fix any remaining call sites.

- [ ] **Step 9: Run the test**

```bash
cargo test -p agent_ui test_restore_terminal_writes_claude_resume 2>&1 | tail -15
```

Expected: test passes.

- [ ] **Step 10: Run the full agent_ui test suite**

```bash
cargo test -p agent_ui 2>&1 | tail -20
```

Expected: all existing tests still pass.

- [ ] **Step 11: Commit**

```bash
git add crates/agent_ui/src/agent_panel.rs
git commit -m "agent_ui: Auto-resume Claude Code session when restoring terminal thread from sidebar"
```

---

### Task 6: Manual smoke test

- [ ] **Step 1: Build Zed in dev mode**

```bash
cargo run 2>&1 | head -5
# or
cargo build && ./target/debug/zed
```

- [ ] **Step 2: Open a project and create a New Terminal Thread in the agent sidebar**

1. Open Zed on a project directory
2. Click the agent sidebar "+" → "New Terminal Thread"
3. In the terminal, type `claude` and press Enter
4. Let Claude Code start and begin a session

- [ ] **Step 3: Verify session ID was captured**

1. Watch the Zed logs: `tail -f ~/Library/Logs/Zed/Zed.log | grep -i claude`
2. The metadata store should have saved the session ID for that terminal thread
3. Alternatively: check `~/.local/share/zed/` (or the relevant DB path) to confirm

- [ ] **Step 4: Test resume**

1. Close the terminal thread (click X on the thread entry)
2. Click on the same thread entry in the sidebar history
3. A new terminal should open and automatically run `claude --resume <session-id>`
4. Claude Code should resume the previous session

- [ ] **Step 5: Final commit (only if any smoke-test fixes were needed)**

```bash
git add -p
git commit -m "agent_ui: Fix claude session watcher smoke test issues"
```
