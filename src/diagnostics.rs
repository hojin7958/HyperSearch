//! Bounded diagnostic records. Never log search queries or complete file lists.
use std::{
    io::Write,
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};
pub fn record(directory: &Path, event: &str, details: serde_json::Value) {
    static LOCK: Mutex<()> = Mutex::new(());
    let Ok(_guard) = LOCK.lock() else { return };
    let path = directory.join("indexer-events.jsonl");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 2 * 1024 * 1024) {
        let _ = std::fs::rename(&path, directory.join("indexer-events.previous.jsonl"));
    }
    let line = serde_json::json!({"time": SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(), "event":event, "details":details});
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}
