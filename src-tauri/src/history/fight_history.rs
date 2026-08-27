use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use tracing::info;

use crate::entity::fight_record::{FightRecord, FightSummary};

/// Cheap fingerprint of the history directory: how many fight files there are
/// and the newest write among them. Comparing this costs a directory scan;
/// rebuilding the summaries costs reading and parsing every file.
#[derive(PartialEq, Eq)]
struct DirStamp {
    count: usize,
    latest: Option<SystemTime>,
}

/// Manages saving and loading fight records as JSON files.
pub struct FightHistoryManager {
    history_dir: PathBuf,
    /// Summaries are distilled from every file in `history_dir` — around 13MB
    /// of JSON parsed in full to keep twelve fields per fight, which measured
    /// at ~350ms. The frontend asks for this once per window at startup and
    /// again every 10s in each window, so the answer is cached and rebuilt only
    /// when the directory actually changes.
    cache: Mutex<Option<(DirStamp, Vec<FightSummary>)>>,
}

impl FightHistoryManager {
    pub fn new(app_data_dir: PathBuf) -> Self {
        let history_dir = app_data_dir.join("history");
        let _ = std::fs::create_dir_all(&history_dir);
        Self {
            history_dir,
            cache: Mutex::new(None),
        }
    }

    fn stamp(&self) -> DirStamp {
        let mut count = 0usize;
        let mut latest: Option<SystemTime> = None;
        if let Ok(entries) = std::fs::read_dir(&self.history_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    count += 1;
                    if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                        latest = Some(match latest {
                            Some(current) if current >= modified => current,
                            _ => modified,
                        });
                    }
                }
            }
        }
        DirStamp { count, latest }
    }

    fn invalidate(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            *cache = None;
        }
    }

    pub fn save_fight(&self, record: &FightRecord) -> Result<(), String> {
        let file_path = self.history_dir.join(format!("{}.json", record.id));
        let json = serde_json::to_string_pretty(record)
            .map_err(|e| format!("Serialization error: {}", e))?;
        std::fs::write(&file_path, json)
            .map_err(|e| format!("Write error: {}", e))?;
        self.invalidate();
        info!("Fight saved: {}", record.id);
        Ok(())
    }

    pub fn load_fight(&self, id: &str) -> Result<FightRecord, String> {
        let file_path = self.history_dir.join(format!("{}.json", id));
        let json = std::fs::read_to_string(&file_path)
            .map_err(|e| format!("Read error: {}", e))?;
        serde_json::from_str(&json)
            .map_err(|e| format!("Parse error: {}", e))
    }

    pub fn delete_fight(&self, id: &str) -> Result<(), String> {
        let file_path = self.history_dir.join(format!("{}.json", id));
        std::fs::remove_file(&file_path)
            .map_err(|e| format!("Delete error: {}", e))?;
        self.invalidate();
        info!("Fight deleted: {}", id);
        Ok(())
    }

    pub fn list_fights(&self) -> Vec<FightSummary> {
        let stamp = self.stamp();
        if let Ok(cache) = self.cache.lock() {
            if let Some((cached, summaries)) = cache.as_ref() {
                if *cached == stamp {
                    return summaries.clone();
                }
            }
        }
        let summaries = self.read_all_summaries();
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some((stamp, summaries.clone()));
        }
        summaries
    }

    fn read_all_summaries(&self) -> Vec<FightSummary> {
        let mut summaries = Vec::new();
        let entries = match std::fs::read_dir(&self.history_dir) {
            Ok(e) => e,
            Err(_) => return summaries,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                if let Ok(json) = std::fs::read_to_string(&path) {
                    if let Ok(record) = serde_json::from_str::<FightRecord>(&json) {
                        summaries.push(FightSummary {
                            id: record.id,
                            boss_name: record.boss_name,
                            target_id: record.target_id,
                            start_time_ms: record.start_time_ms,
                            duration_ms: record.duration_ms,
                            total_damage: record.total_damage,
                            jobs: record.jobs,
                            job_ids: record.job_ids,
                            is_train: record.is_train,
                            is_live: false,
                            app_version: record.app_version,
                            mob_code: record.mob_code,
                        });
                    }
                }
            }
        }

        summaries.sort_by(|a, b| b.start_time_ms.cmp(&a.start_time_ms));
        summaries
    }

    pub fn export_fight_json(&self, record: &FightRecord) -> Result<String, String> {
        serde_json::to_string(record)
            .map_err(|e| format!("Serialization error: {}", e))
    }
}
