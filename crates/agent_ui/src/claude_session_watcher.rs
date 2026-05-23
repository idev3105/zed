use std::{path::PathBuf, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use futures::StreamExt as _;
use gpui::{AsyncApp, Task, WeakEntity};
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
    #[allow(deprecated)]
    let home = std::env::home_dir()?;
    let encoded = working_dir.to_str()?.replace('/', "-");
    Some(home.join(".claude").join("projects").join(encoded))
}

/// The watch loop run as a GPUI foreground task.
/// Callers construct the task with `cx.spawn(...)` and store it in `ClaudeSessionWatcher { _task }`.
pub async fn watch_loop(
    terminal_id: TerminalId,
    created_at: DateTime<Utc>,
    watch_dir: PathBuf,
    fs: Arc<dyn fs::Fs>,
    store: WeakEntity<TerminalThreadMetadataStore>,
    cx: &mut AsyncApp,
) {
    // Poll for up to 30 seconds for the directory to appear (Claude Code may not have run yet).
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

            let Ok(Some(metadata)) = fs.metadata(&event.path).await else {
                continue;
            };
            let file_time = metadata.mtime.timestamp_for_user();
            let file_dt = chrono::DateTime::<Utc>::from(file_time);
            if file_dt <= created_at {
                continue;
            }

            let claimed = store
                .update(cx, |store, cx| {
                    store.try_claim_session(terminal_id, created_at, session_id, cx)
                })
                .unwrap_or(false);

            if claimed {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_thread_metadata_store::TerminalThreadMetadata;
    use crate::thread_metadata_store::WorktreePaths;
    use fs::FakeFs;
    use gpui::TestAppContext;

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
        let meta = make_metadata(terminal_id, "/home/user/myproject");
        let created_at = meta.created_at;

        cx.update(|cx| store.update(cx, |s, cx| s.save(meta, cx)));

        let fake_fs = FakeFs::new(cx.background_executor().clone());
        let watch_dir = PathBuf::from("/fake-home/.claude/projects/-home-user-myproject");
        fake_fs.create_dir(&watch_dir).await.unwrap();

        let fs: Arc<dyn fs::Fs> = Arc::new(fake_fs.clone());
        let store_weak = store.downgrade();

        let _watcher_task = cx.spawn(async move |mut cx| {
            watch_loop(terminal_id, created_at, watch_dir.clone(), fs, store_weak, &mut cx).await
        });

        cx.run_until_parked();

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

        // Insert a file before starting the watcher — it won't fire a Created event
        // because the watcher only picks up events after watch() is called.
        fake_fs
            .insert_file(
                "/fake-home/.claude/projects/-home-user-project/old-session.jsonl",
                "{}".into(),
            )
            .await;

        let fs: Arc<dyn fs::Fs> = Arc::new(fake_fs.clone());
        let store_weak = store.downgrade();

        let _watcher_task = cx.spawn(async move |mut cx| {
            watch_loop(terminal_id, created_at, watch_dir, fs, store_weak, &mut cx).await
        });

        cx.run_until_parked();

        let saved_id = cx.update(|cx| {
            store
                .read(cx)
                .entry(terminal_id)
                .and_then(|m| m.claude_session_id.clone())
        });
        assert_eq!(saved_id, None);
    }
}
