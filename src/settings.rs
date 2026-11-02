use crate::{
    engine::{EntryKind, SearchMode, SortField},
    indexer, platform,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub roots: Vec<PathBuf>,
    pub excluded_folders: Vec<PathBuf>,
    pub hotkey: String,
    pub font_size: f32,
    pub window_size: [f32; 2],
    pub window_pos: Option<[f32; 2]>,
    pub maximized: bool,
    pub query: String,
    pub column_widths: [i32; 5],
    pub match_path: bool,
    pub case_sensitive: bool,
    pub mode: SearchMode,
    pub kind: EntryKind,
    pub sort: SortField,
    pub descending: bool,
    pub fast_index: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            roots: indexer::default_roots(),
            excluded_folders: Vec::new(),
            hotkey: "Ctrl+Alt+1".into(),
            font_size: 9.0,
            window_size: [960.0, 560.0],
            window_pos: None,
            maximized: false,
            query: String::new(),
            column_widths: [200, 240, 80, 165, 165],
            match_path: false,
            case_sensitive: false,
            mode: SearchMode::Plain,
            kind: EntryKind::All,
            sort: SortField::Modified,
            descending: true,
            fast_index: true,
        }
    }
}

impl Settings {
    pub fn load() -> Self {
        let path = platform::local_data_dir().join("settings.json");
        let mut value = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .unwrap_or_default();
        value.font_size = value.font_size.clamp(8.0, 20.0);
        value.window_size[0] = value.window_size[0].clamp(600.0, 3840.0);
        value.window_size[1] = value.window_size[1].clamp(320.0, 2160.0);
        for width in &mut value.column_widths {
            *width = (*width).clamp(50, 2000);
        }
        value
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = platform::local_data_dir();
        std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        let data = serde_json::to_vec_pretty(self).map_err(|err| err.to_string())?;
        let temporary = dir.join("settings.json.tmp");
        std::fs::write(&temporary, data).map_err(|err| err.to_string())?;
        std::fs::rename(temporary, dir.join("settings.json")).map_err(|err| err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_settings_keep_font_query_and_roots_when_exclusions_are_added() {
        let mut settings: Settings = serde_json::from_value(serde_json::json!({
            "roots": ["C:\\資料"], "font_size": 13.0, "query": "*보고서*.xlsx", "hotkey": "Ctrl+Alt+2"
        })).unwrap();
        assert!(settings.excluded_folders.is_empty());
        settings.excluded_folders = vec![r"C:\資料\제외".into()];
        let restored: Settings =
            serde_json::from_slice(&serde_json::to_vec(&settings).unwrap()).unwrap();
        assert_eq!(restored.excluded_folders, settings.excluded_folders);
        assert_eq!(restored.font_size, 13.0);
        assert_eq!(restored.query, "*보고서*.xlsx");
        assert_eq!(restored.hotkey, "Ctrl+Alt+2");
        assert_eq!(restored.roots, [PathBuf::from(r"C:\資料")]);
    }
}
