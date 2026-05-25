use std::{path::PathBuf, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use futures::StreamExt as _;
use gpui::{AsyncApp, Task, WeakEntity};
use serde_json::Value;
use ui::SharedString;

use crate::{TerminalId, terminal_thread_metadata_store::TerminalThreadMetadataStore};

/// Wraps the watcher `Task<()>`; dropping this cancels the watcher.
pub struct ClaudeSessionWatcher {
    pub _task: Task<()>,
}

/// Returns the path Claude Code uses to store sessions for the given working directory.
/// Claude Code encodes the project path by replacing path separators with '-'.
/// On Unix: /Users/john/code -> -Users-john-code
/// On Windows: C:\Users\john\code -> C:-Users-john-code
pub fn claude_session_dir_for(working_dir: &std::path::Path) -> Option<PathBuf> {
    let home = paths::home_dir();
    let encoded = working_dir.to_str()?.replace(['/', '\\'], "-");
    Some(home.join(".claude").join("projects").join(encoded))
}

/// The watch loop run as a GPUI foreground task.
/// Callers construct the task with `cx.spawn(...)` and store it in `ClaudeSessionWatcher { _task }`.
/// After a session is claimed, polls the session's `.jsonl` file for an `ai-title` entry and
/// calls `on_ai_title` with the title string when one is found.
pub async fn watch_loop(
    terminal_id: TerminalId,
    created_at: DateTime<Utc>,
    watch_dir: PathBuf,
    fs: Arc<dyn fs::Fs>,
    store: WeakEntity<TerminalThreadMetadataStore>,
    on_ai_title: Box<dyn FnOnce(SharedString, &mut AsyncApp) + Send + 'static>,
    cx: &mut AsyncApp,
) {
    log::debug!(
        "claude_session_watcher [{:?}]: start, watch_dir={:?}",
        terminal_id,
        watch_dir
    );

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
            log::debug!(
                "claude_session_watcher [{:?}]: session dir found, starting fs watch",
                terminal_id
            );
            break;
        }
        if std::time::Instant::now() >= deadline {
            log::debug!(
                "claude_session_watcher [{:?}]: timed out waiting for session dir {:?}",
                terminal_id,
                watch_dir
            );
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

            log::debug!(
                "claude_session_watcher [{:?}]: new .jsonl detected, session_id={}",
                terminal_id,
                session_id
            );

            let Ok(Some(metadata)) = fs.metadata(&event.path).await else {
                log::debug!(
                    "claude_session_watcher [{:?}]: could not read metadata for {:?}, skipping",
                    terminal_id,
                    event.path
                );
                continue;
            };
            let file_time = metadata.mtime.timestamp_for_user();
            let file_dt = chrono::DateTime::<Utc>::from(file_time);
            if file_dt <= created_at {
                log::debug!(
                    "claude_session_watcher [{:?}]: skipping old file (file_time={} <= terminal_created_at={})",
                    terminal_id,
                    file_dt,
                    created_at
                );
                continue;
            }

            let session_file = watch_dir.join(format!("{}.jsonl", session_id));
            let claimed = store
                .update(cx, |store, cx| {
                    store.try_claim_session(terminal_id, created_at, session_id.clone(), cx)
                })
                .unwrap_or(false);

            log::debug!(
                "claude_session_watcher [{:?}]: try_claim_session({}) -> claimed={}",
                terminal_id,
                session_id,
                claimed
            );

            if claimed {
                poll_ai_title(terminal_id, session_file, fs, on_ai_title, cx).await;
                return;
            }
        }
    }
}

async fn poll_ai_title(
    terminal_id: TerminalId,
    session_file: PathBuf,
    fs: Arc<dyn fs::Fs>,
    on_ai_title: Box<dyn FnOnce(SharedString, &mut AsyncApp) + Send + 'static>,
    cx: &mut AsyncApp,
) {
    log::debug!(
        "claude_session_watcher [{:?}]: polling for ai-title in {:?}",
        terminal_id,
        session_file
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        if std::time::Instant::now() >= deadline {
            log::debug!(
                "claude_session_watcher [{:?}]: timed out waiting for ai-title",
                terminal_id
            );
            return;
        }
        if let Ok(content) = fs.load(&session_file).await {
            for line in content.lines() {
                if !line.contains("\"ai-title\"") {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(line) {
                    if value["type"].as_str() == Some("ai-title") {
                        if let Some(title) = value["aiTitle"].as_str() {
                            log::debug!(
                                "claude_session_watcher [{:?}]: found ai-title={:?}, applying",
                                terminal_id,
                                title
                            );
                            on_ai_title(SharedString::from(title.to_owned()), cx);
                            return;
                        }
                    }
                }
            }
        }
        cx.background_executor().timer(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_thread_metadata_store::TerminalThreadMetadata;
    use crate::thread_metadata_store::WorktreePaths;
    use fs::{FakeFs, Fs as _};
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

        let fake_fs = FakeFs::new(cx.background_executor.clone());
        let watch_dir = PathBuf::from("/fake-home/.claude/projects/-home-user-myproject");
        fake_fs.create_dir(&watch_dir).await.unwrap();

        let fs: Arc<dyn fs::Fs> = fake_fs.clone();
        let store_weak = store.downgrade();

        let _watcher_task = cx.spawn(async move |mut cx| {
            watch_loop(
                terminal_id,
                created_at,
                watch_dir.clone(),
                fs,
                store_weak,
                Box::new(|_title, _cx| {}),
                &mut cx,
            )
            .await
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

        let fake_fs = FakeFs::new(cx.background_executor.clone());
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

        let fs: Arc<dyn fs::Fs> = fake_fs.clone();
        let store_weak = store.downgrade();

        let _watcher_task = cx.spawn(async move |mut cx| {
            watch_loop(
                terminal_id,
                created_at,
                watch_dir,
                fs,
                store_weak,
                Box::new(|_title, _cx| {}),
                &mut cx,
            )
            .await
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

    #[gpui::test]
    async fn test_watcher_calls_on_ai_title(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::terminal_thread_metadata_store::TerminalThreadMetadataStore::init_global(cx)
        });

        let store = cx.update(|cx| TerminalThreadMetadataStore::global(cx));
        let terminal_id = TerminalId::new();
        let meta = make_metadata(terminal_id, "/home/user/aiproj");
        let created_at = meta.created_at;
        cx.update(|cx| store.update(cx, |s, cx| s.save(meta, cx)));

        let fake_fs = FakeFs::new(cx.background_executor.clone());
        let watch_dir = PathBuf::from("/fake-home/.claude/projects/-home-user-aiproj");
        fake_fs.create_dir(&watch_dir).await.unwrap();

        let fs: Arc<dyn fs::Fs> = fake_fs.clone();
        let store_weak = store.downgrade();

        let received_title: std::sync::Arc<std::sync::Mutex<Option<SharedString>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let received_title_clone = received_title.clone();
        let on_ai_title = Box::new(move |title: SharedString, _cx: &mut AsyncApp| {
            *received_title_clone.lock().unwrap() = Some(title);
        });

        let _watcher_task = cx.spawn(async move |mut cx| {
            watch_loop(
                terminal_id,
                created_at,
                watch_dir.clone(),
                fs,
                store_weak,
                on_ai_title,
                &mut cx,
            )
            .await
        });

        cx.run_until_parked();

        // Write the session file with an ai-title entry
        fake_fs
            .insert_file(
                "/fake-home/.claude/projects/-home-user-aiproj/sess-abc.jsonl",
                concat!(
                    "{\"type\":\"user\",\"sessionId\":\"sess-abc\"}\n",
                    "{\"type\":\"ai-title\",\"aiTitle\":\"Fix login bug\",\"sessionId\":\"sess-abc\"}\n"
                )
                .into(),
            )
            .await;

        cx.run_until_parked();

        let title = received_title.lock().unwrap().clone();
        assert_eq!(title, Some(SharedString::from("Fix login bug")));
    }
}
