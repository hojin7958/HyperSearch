//! Small cumulative metadata patches relative to a committed service snapshot.
use crate::model::{Entry, Index};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::Path,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub entry: Option<Entry>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Delta {
    pub base: u128,
    pub revision: u128,
    pub updated: u64,
    #[serde(default)]
    pub request_id: u128,
    pub changes: Vec<Change>,
}
pub fn read(path: &Path) -> Result<Delta, String> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(32 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 32 * 1024 * 1024 {
        return Err("변경 목록 크기 한도 초과".into());
    }
    let delta: Delta = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if delta.changes.len() > 65_536 {
        return Err("변경 목록 항목 한도 초과".into());
    }
    Ok(delta)
}
pub fn apply(index: &Index, delta: &Delta) -> Result<Index, String> {
    let mut updates = HashMap::<(u32, String), Option<Entry>>::new();
    for change in &delta.changes {
        let path = Path::new(&change.path);
        let parent = path.parent().ok_or("변경 항목 상위 경로 오류")?;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("변경 항목 이름 오류")?;
        if name.contains(['\\', '/', '\0']) || name.is_empty() || !path.is_absolute() {
            return Err("변경 항목 경로 오류".into());
        }
        let Some(parent) = index.folders.find_path(&parent.to_string_lossy()) else {
            if change.entry.is_none() {
                continue;
            }
            return Err("폴더 구조가 달라 새 기준 목록이 필요합니다.".into());
        };
        let entry = match &change.entry {
            Some(entry) => {
                if entry.name != name {
                    return Err("변경 항목 이름 불일치".into());
                }
                let mut entry = entry.clone();
                entry.parent = parent;
                Some(entry)
            }
            None => None,
        };
        updates.insert((parent, name.to_lowercase()), entry);
    }
    let mut changed = index.clone();
    let names: HashMap<u32, HashSet<&str>> =
        updates
            .keys()
            .fold(HashMap::new(), |mut grouped, (parent, name)| {
                grouped.entry(*parent).or_default().insert(name.as_str());
                grouped
            });
    changed.entries.retain(|entry| {
        !names
            .get(&entry.parent)
            .is_some_and(|names| names.contains(entry.name.to_lowercase().as_str()))
    });
    for entry in updates.into_values().flatten() {
        changed.entries.push(entry);
    }
    changed.built_at = delta.updated as i64;
    Ok(changed)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cumulative_patches_are_idempotent_and_preserve_previous_snapshot() {
        let mut index = Index::default();
        index.folders.push("C:\\files".into());
        index.entries.push(Entry {
            name: "old.txt".into(),
            parent: 0,
            size: Some(1),
            created: None,
            modified: None,
            is_dir: false,
        });
        index.share();
        let delta = Delta {
            changes: vec![
                Change {
                    path: "C:\\files\\old.txt".into(),
                    entry: None,
                },
                Change {
                    path: "C:\\files\\new.txt".into(),
                    entry: Some(Entry {
                        name: "new.txt".into(),
                        parent: 99,
                        size: Some(2),
                        created: Some(-1),
                        modified: Some(i64::MAX),
                        is_dir: false,
                    }),
                },
            ],
            ..Delta::default()
        };
        let first = apply(&index, &delta).unwrap();
        let second = apply(&first, &delta).unwrap();
        assert_eq!(index.entry(0).name, "old.txt");
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(second.full_path(0), "C:\\files\\new.txt");
        assert_eq!(second.entry(0).modified, Some(i64::MAX));
    }
}
