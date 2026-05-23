use crate::bus::{
    BackgroundTaskProgress, BackgroundTaskProgressEvent, BackgroundTaskProgressKind,
    BackgroundTaskProgressSource, Bus, BusEvent,
};
use chrono::Utc;
use jcode_codebase_sync::{CodebaseSyncEngine, ManifestStore, workspace_id};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

static STARTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

pub fn start_for_session(session_id: String, working_dir: Option<String>) {
    if !crate::config::config().features.codebase_sync {
        return;
    }
    let Some(root) = working_dir.map(PathBuf::from).filter(|path| path.exists()) else {
        return;
    };
    let key = format!("{}:{}", session_id, root.display());
    let started = STARTED.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut started) = started.lock() {
        if !started.insert(key) {
            return;
        }
    }

    std::thread::spawn(move || {
        run_runtime(session_id, root);
    });
}

fn run_runtime(session_id: String, root: PathBuf) {
    let task_id = format!("codebase-sync-{}", workspace_id(&root));
    publish_progress(&session_id, &task_id, "scanning", 0.0, 0, 1);
    let engine = match ManifestStore::default_store().map(CodebaseSyncEngine::new) {
        Ok(engine) => engine,
        Err(err) => {
            publish_error(&session_id, &task_id, &format!("init failed: {err}"));
            return;
        }
    };

    match engine.open_workspace(&root) {
        Ok((manifest, _delta)) => {
            publish_progress(
                &session_id,
                &task_id,
                "indexed",
                100.0,
                manifest.files.len() as u64,
                manifest.files.len().max(1) as u64,
            );
        }
        Err(err) => {
            publish_error(&session_id, &task_id, &format!("scan failed: {err}"));
            return;
        }
    }

    match engine.start_native_workspace_watcher(root, Duration::from_millis(500)) {
        Ok(_watcher) => loop {
            std::thread::park();
        },
        Err(err) => {
            publish_error(&session_id, &task_id, &format!("watch failed: {err}"));
        }
    }
}

fn publish_error(session_id: &str, task_id: &str, message: &str) {
    publish_progress(session_id, task_id, message, 100.0, 1, 1);
}

fn publish_progress(
    session_id: &str,
    task_id: &str,
    message: &str,
    percent: f32,
    current: u64,
    total: u64,
) {
    Bus::global().publish(BusEvent::BackgroundTaskProgress(
        BackgroundTaskProgressEvent {
            task_id: task_id.to_string(),
            tool_name: "codebase_sync".to_string(),
            display_name: Some("Codebase sync".to_string()),
            session_id: session_id.to_string(),
            progress: BackgroundTaskProgress {
                kind: BackgroundTaskProgressKind::Determinate,
                percent: Some(percent),
                message: Some(message.to_string()),
                current: Some(current),
                total: Some(total),
                unit: Some("files".to_string()),
                eta_seconds: None,
                updated_at: Utc::now().to_rfc3339(),
                source: BackgroundTaskProgressSource::Reported,
            }
            .normalize(),
        },
    ));
}

pub fn start_for_path(session_id: impl Into<String>, root: &Path) {
    start_for_session(session_id.into(), Some(root.to_string_lossy().into_owned()));
}
