//! A bounded, versioned binary cache. Lengths are checked against the actual
//! remaining file bytes before allocation, rather than trusting serde lengths.
use crate::model::{Entry, EntryStore, FolderNode, FolderStore, Index, Record};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const MAGIC: &[u8; 8] = b"QFIDX\0\0\0";
const VERSION: u32 = 3;
const MAX_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000_000;
const MAX_FOLDERS: usize = 2_000_000;
const MAX_ROOTS: usize = 256;
const MAX_PATH_BYTES: usize = 128 * 1024;
const MAX_NAME_BYTES: usize = 32 * 1024;

pub fn load(path: &Path, roots: &[PathBuf]) -> Result<Index, String> {
    let file = File::open(path).map_err(|e| format!("인덱스 열기 실패: {e}"))?;
    let len = file.metadata().map_err(err)?.len();
    if !(48..=MAX_BYTES).contains(&len) {
        return Err("인덱스 파일 크기가 올바르지 않습니다".into());
    }
    let mut input = Input {
        reader: BufReader::new(file),
        remaining: len,
    };
    if input.bytes::<8>()? != *MAGIC {
        return Err("지원하지 않는 인덱스 형식입니다".into());
    }
    let version = input.u32()?;
    if !(1..=VERSION).contains(&version) {
        return Err("지원하지 않는 인덱스 형식입니다".into());
    }
    let scan_ms = input.u64()?;
    let skipped = input.u64()?;
    let built_at = input.i64()?;
    let cached_roots = input.strings(MAX_ROOTS, MAX_PATH_BYTES)?;
    let expected: Vec<String> = roots
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect();
    if normalized_roots(&cached_roots) != normalized_roots(&expected) {
        return Err("검색 범위가 변경되어 인덱스를 다시 만듭니다".into());
    }
    let excluded_folders = if version >= 3 {
        input.strings(MAX_ROOTS, MAX_PATH_BYTES)?
    } else {
        Vec::new()
    };
    if version >= 2 {
        return load_compact(
            input,
            cached_roots,
            excluded_folders,
            scan_ms,
            skipped,
            built_at,
        );
    }
    // Version 1 migration is streamed directly into the compact stores. Never
    // materialize millions of String/Option-rich legacy records at once.
    let folder_count = input.count(MAX_FOLDERS, 4)?;
    let mut folders = FolderStore::default();
    for _ in 0..folder_count {
        let folder = input.string(MAX_PATH_BYTES)?;
        if folder.is_empty() {
            return Err("인덱스에 빈 경로가 있습니다".into());
        }
        folders.push(folder);
    }
    let count = input.count(MAX_ENTRIES, 9)?;
    let mut entries = EntryStore::default();
    entries.records.try_reserve_exact(count).map_err(err)?;
    for _ in 0..count {
        let name = input.string(MAX_NAME_BYTES)?;
        if name.is_empty() {
            return Err("빈 파일명이 포함된 인덱스입니다".into());
        }
        let parent = input.u32()?;
        if parent as usize >= folders.len() {
            return Err("인덱스의 상위 폴더가 올바르지 않습니다".into());
        }
        let flags = input.bytes::<1>()?[0];
        if flags & !15 != 0 {
            return Err("인덱스 항목 형식이 올바르지 않습니다".into());
        }
        let size = if flags & 1 != 0 {
            Some(input.u64()?)
        } else {
            None
        };
        let created = if flags & 2 != 0 {
            Some(input.i64()?)
        } else {
            None
        };
        let modified = if flags & 4 != 0 {
            Some(input.i64()?)
        } else {
            None
        };
        entries.push(Entry {
            name,
            parent,
            size,
            created,
            modified,
            is_dir: flags & 8 != 0,
        });
    }
    if input.remaining != 0 {
        return Err("인덱스 파일에 예상하지 못한 데이터가 있습니다".into());
    }
    let mut index = Index {
        entries,
        folders,
        roots: cached_roots,
        excluded_folders,
        scan_ms,
        skipped,
        built_at,
    };
    index.compact();
    Ok(index)
}

/// Reuse a cached scope only when it cannot omit a newly included folder.
/// Adding exclusions can filter an old snapshot immediately; removing one
/// requires a fresh sync, even when automatic startup sync is disabled.
/// Commit a narrowed scope so a later removal cannot reuse stale hidden rows.
pub fn load_with_exclusions(
    path: &Path,
    roots: &[PathBuf],
    excluded: &[PathBuf],
) -> Result<Index, String> {
    let mut index = load(path, roots)?;
    let rules = crate::exclusions::FolderExclusions::new(excluded);
    let previous = crate::exclusions::FolderExclusions::from_strings(&index.excluded_folders);
    if !rules.covers(&previous) {
        return Err("제외 폴더 설정이 변경되어 인덱스를 다시 동기화합니다.".into());
    }
    if rules.paths() != previous.paths() {
        rules.apply(&mut index);
        index.compact();
        save(path, &index)?;
    }
    Ok(index)
}

fn load_compact(
    mut input: Input,
    roots: Vec<String>,
    excluded_folders: Vec<String>,
    scan_ms: u64,
    skipped: u64,
    built_at: i64,
) -> Result<Index, String> {
    let folder_count = input.u32()? as usize;
    let node_count = input.u32()? as usize;
    let folder_bytes = input.u32()? as usize;
    let count = input.u32()? as usize;
    let name_bytes = input.u32()? as usize;
    let wide_count = input.u32()? as usize;
    let needed = folder_count as u64 * 4
        + node_count as u64 * 12
        + folder_bytes as u64
        + count as u64 * 28
        + name_bytes as u64
        + wide_count as u64 * 12;
    if folder_count > MAX_FOLDERS
        || node_count > MAX_ENTRIES
        || count > MAX_ENTRIES
        || wide_count > count.saturating_mul(2)
        || needed != input.remaining
    {
        return Err("압축 인덱스 길이가 올바르지 않습니다".into());
    }
    let mut folders = FolderStore::default();
    folders.ids.try_reserve_exact(folder_count).map_err(err)?;
    for _ in 0..folder_count {
        folders.ids.push(input.u32()?);
    }
    folders.nodes.try_reserve_exact(node_count).map_err(err)?;
    for _ in 0..node_count {
        folders.nodes.push(FolderNode {
            parent: input.u32()?,
            offset: input.u32()?,
            len: input.u32()?,
        });
    }
    folders.names = input.buffer(folder_bytes)?;
    let mut entries = EntryStore::default();
    entries.records.try_reserve_exact(count).map_err(err)?;
    entries.records.resize(count, Record([0; 7]));
    // Record is a repr-transparent array of u32 (28 bytes, no padding).
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(entries.records.as_mut_ptr().cast::<u8>(), count * 28)
    };
    input.read_buffer(bytes)?;
    if cfg!(target_endian = "big") {
        for r in &mut entries.records {
            for v in &mut r.0 {
                *v = u32::from_le(*v);
            }
        }
    }
    entries.names = input.buffer(name_bytes)?;
    entries
        .wide_times
        .try_reserve_exact(wide_count)
        .map_err(err)?;
    for _ in 0..wide_count {
        entries.wide_times.push((input.u32()?, input.i64()?));
    }
    let index = Index {
        entries,
        folders,
        roots,
        excluded_folders,
        scan_ms,
        skipped,
        built_at,
    };
    validate(&index)?;
    Ok(index)
}

pub fn save(path: &Path, index: &Index) -> Result<(), String> {
    validate(index)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(err)?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = path.with_extension(format!("tmp.{}.{nonce}", std::process::id()));
    let result = (|| {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(err)?;
        let mut output = BufWriter::new(file);
        output.write_all(MAGIC).map_err(err)?;
        output.write_all(&VERSION.to_le_bytes()).map_err(err)?;
        output
            .write_all(&index.scan_ms.to_le_bytes())
            .map_err(err)?;
        output
            .write_all(&index.skipped.to_le_bytes())
            .map_err(err)?;
        output
            .write_all(&index.built_at.to_le_bytes())
            .map_err(err)?;
        write_strings(&mut output, &index.roots)?;
        write_strings(&mut output, &index.excluded_folders)?;
        let folders = &index.folders;
        let entries = &index.entries;
        for length in [
            folders.ids.len(),
            folders.nodes.len(),
            folders.names.len(),
            entries.records.len(),
            entries.names.len(),
            entries.wide_times.len(),
        ] {
            output
                .write_all(&(length as u32).to_le_bytes())
                .map_err(err)?;
        }
        for &id in &folders.ids {
            output.write_all(&id.to_le_bytes()).map_err(err)?;
        }
        for n in &folders.nodes {
            for value in [n.parent, n.offset, n.len] {
                output.write_all(&value.to_le_bytes()).map_err(err)?;
            }
        }
        output.write_all(&folders.names).map_err(err)?;
        if cfg!(target_endian = "little") {
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    entries.records.as_ptr().cast::<u8>(),
                    entries.len() * 28,
                )
            };
            output.write_all(bytes).map_err(err)?;
        } else {
            for record in &entries.records {
                for value in record.0 {
                    output.write_all(&value.to_le_bytes()).map_err(err)?;
                }
            }
        }
        output.write_all(&entries.names).map_err(err)?;
        for &(row, time) in &entries.wide_times {
            output.write_all(&row.to_le_bytes()).map_err(err)?;
            output.write_all(&time.to_le_bytes()).map_err(err)?;
        }
        output.flush().map_err(err)?;
        output.get_ref().sync_all().map_err(err)?;
        if output.get_ref().metadata().map_err(err)?.len() > MAX_BYTES {
            return Err("인덱스가 캐시 최대 크기를 초과했습니다".into());
        }
        drop(output);
        // On Windows Rust's rename replaces an existing destination file.
        // The old cache remains intact until the complete temp file is synced.
        fs::rename(&temp, path).map_err(err)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn validate(index: &Index) -> Result<(), String> {
    if index.entries.len() > MAX_ENTRIES
        || index.folders.len() > MAX_FOLDERS
        || index.roots.len() > MAX_ROOTS
        || index.excluded_folders.len() > MAX_ROOTS
    {
        return Err("인덱스 항목 수가 최대 범위를 초과했습니다".into());
    }
    let invalid = || "인덱스 경로 또는 항목이 올바르지 않습니다".to_owned();
    if index
        .roots
        .iter()
        .chain(index.excluded_folders.iter())
        .any(|s| s.is_empty() || s.len() > MAX_PATH_BYTES || s.contains('\0'))
    {
        return Err(invalid());
    }
    let folders = &index.folders;
    let entries = &index.entries;
    let folder_text = std::str::from_utf8(&folders.names).map_err(err)?;
    let mut path_lengths = Vec::<u32>::with_capacity(folders.nodes.len());
    for (id, n) in folders.nodes.iter().enumerate() {
        let start = n.offset as usize;
        let end = start.checked_add(n.len as usize).ok_or_else(invalid)?;
        if (n.parent != u32::MAX && n.parent as usize >= id)
            || n.len == 0
            || end > folders.names.len()
            || !folder_text.is_char_boundary(start)
            || !folder_text.is_char_boundary(end)
            || folder_text[start..end].contains('\0')
        {
            return Err(invalid());
        }
        let parent_length = if n.parent == u32::MAX {
            0
        } else {
            path_lengths[n.parent as usize]
        };
        let length = parent_length.checked_add(n.len).ok_or_else(invalid)?;
        if length as usize > MAX_PATH_BYTES {
            return Err(invalid());
        }
        path_lengths.push(length);
    }
    drop(path_lengths);
    if folders
        .ids
        .iter()
        .any(|&id| id as usize >= folders.nodes.len())
    {
        return Err(invalid());
    }
    // Validate the entire length-prefixed pool before any unchecked name view.
    let mut position = 0;
    while position < entries.names.len() {
        if entries.names.len() - position < 4 {
            return Err(invalid());
        }
        let length =
            u32::from_le_bytes(entries.names[position..position + 4].try_into().unwrap()) as usize;
        let end = position.checked_add(4 + length).ok_or_else(invalid)?;
        if length == 0 || length > MAX_NAME_BYTES || end > entries.names.len() {
            return Err(invalid());
        }
        let text = std::str::from_utf8(&entries.names[position + 4..end]).map_err(err)?;
        if text.contains('\0') {
            return Err(invalid());
        }
        position = end;
    }
    let mut last = None;
    for &(key, _) in &entries.wide_times {
        if key as usize >= entries.len() * 2 || last.is_some_and(|old| old >= key) {
            return Err(invalid());
        }
        last = Some(key);
    }
    for (row, record) in entries.records.iter().enumerate() {
        let r = record.0;
        let start = r[0] as usize;
        if start
            .checked_add(4)
            .is_none_or(|end| end > entries.names.len())
            || r[1] as usize >= folders.len()
            || r[6] & !3 != 0
        {
            return Err(invalid());
        }
        let length =
            u32::from_le_bytes(entries.names[start..start + 4].try_into().unwrap()) as usize;
        let end = start.checked_add(4 + length).ok_or_else(invalid)?;
        if length == 0 || length > MAX_NAME_BYTES || end > entries.names.len() {
            return Err(invalid());
        }
        if std::str::from_utf8(&entries.names[start + 4..end])
            .map_err(err)?
            .contains('\0')
        {
            return Err(invalid());
        }
        for (kind, time) in [(0, r[4]), (1, r[5])] {
            if time == u32::MAX - 1
                && entries
                    .wide_times
                    .binary_search_by_key(&(row as u32 * 2 + kind), |&(k, _)| k)
                    .is_err()
            {
                return Err(invalid());
            }
        }
    }
    let bytes = 68
        + folders.ids.len() as u64 * 4
        + folders.nodes.len() as u64 * 12
        + folders.names.len() as u64
        + entries.len() as u64 * 28
        + entries.names.len() as u64
        + entries.wide_times.len() as u64 * 12
        + index
            .roots
            .iter()
            .chain(index.excluded_folders.iter())
            .map(|r| 4 + r.len() as u64)
            .sum::<u64>();
    if bytes > MAX_BYTES {
        return Err("인덱스가 캐시 최대 크기를 초과했습니다".into());
    }
    Ok(())
}

fn normalized_roots(roots: &[String]) -> Vec<String> {
    let mut roots: Vec<_> = roots
        .iter()
        .map(|r| r.replace('/', "\\").trim_end_matches('\\').to_lowercase())
        .collect();
    roots.sort_unstable();
    roots.dedup();
    roots
}

struct Input {
    reader: BufReader<File>,
    remaining: u64,
}
impl Input {
    fn read_buffer(&mut self, bytes: &mut [u8]) -> Result<(), String> {
        if bytes.len() as u64 > self.remaining {
            return Err("인덱스 파일이 불완전합니다".into());
        }
        self.reader.read_exact(bytes).map_err(err)?;
        self.remaining -= bytes.len() as u64;
        Ok(())
    }
    fn buffer(&mut self, length: usize) -> Result<Vec<u8>, String> {
        if length as u64 > self.remaining {
            return Err("인덱스 파일이 불완전합니다".into());
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(err)?;
        bytes.resize(length, 0);
        self.read_buffer(&mut bytes)?;
        Ok(bytes)
    }
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], String> {
        if self.remaining < N as u64 {
            return Err("인덱스 파일이 불완전합니다".into());
        }
        let mut bytes = [0; N];
        self.reader.read_exact(&mut bytes).map_err(err)?;
        self.remaining -= N as u64;
        Ok(bytes)
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.bytes()?))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.bytes()?))
    }
    fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_le_bytes(self.bytes()?))
    }
    fn count(&mut self, max: usize, minimum_bytes: u64) -> Result<usize, String> {
        let count = self.u32()? as usize;
        if count > max || count as u64 > self.remaining / minimum_bytes {
            return Err("인덱스 항목 수가 올바르지 않습니다".into());
        }
        Ok(count)
    }
    fn string(&mut self, max: usize) -> Result<String, String> {
        let len = self.u32()? as usize;
        if len > max || len as u64 > self.remaining {
            return Err("인덱스 문자열 길이가 올바르지 않습니다".into());
        }
        let mut bytes = vec![0; len];
        self.reader.read_exact(&mut bytes).map_err(err)?;
        self.remaining -= len as u64;
        let value = String::from_utf8(bytes).map_err(err)?;
        if value.contains('\0') {
            return Err("인덱스 경로가 올바르지 않습니다".into());
        }
        Ok(value)
    }
    fn strings(&mut self, max: usize, max_bytes: usize) -> Result<Vec<String>, String> {
        let count = self.count(max, 4)?;
        let mut strings = Vec::new();
        strings.try_reserve_exact(count).map_err(err)?;
        for _ in 0..count {
            let value = self.string(max_bytes)?;
            if value.is_empty() {
                return Err("인덱스에 빈 경로가 있습니다".into());
            }
            strings.push(value);
        }
        Ok(strings)
    }
}

fn write_string(output: &mut impl Write, value: &str) -> Result<(), String> {
    output
        .write_all(&(value.len() as u32).to_le_bytes())
        .map_err(err)?;
    output.write_all(value.as_bytes()).map_err(err)
}
fn write_strings(output: &mut impl Write, values: &[String]) -> Result<(), String> {
    output
        .write_all(&(values.len() as u32).to_le_bytes())
        .map_err(err)?;
    for value in values {
        write_string(output, value)?;
    }
    Ok(())
}
fn err(error: impl std::fmt::Display) -> String {
    format!("인덱스 캐시 오류: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Index {
        Index {
            roots: vec!["C:\\자료".into()],
            folders: vec!["C:\\자료".into()].into(),
            entries: vec![Entry {
                name: "한글.txt".into(),
                parent: 0,
                size: Some(42),
                created: Some(-1),
                modified: None,
                is_dir: false,
            }]
            .into_iter()
            .collect(),
            scan_ms: 100,
            skipped: 2,
            built_at: 12345,
            excluded_folders: Vec::new(),
        }
    }
    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!(
            "quickfind-cache-test-{}-{}.bin",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
    #[test]
    fn roundtrip_replace_and_root_validation() {
        let path = temp();
        let index = fixture();
        save(&path, &index).unwrap();
        save(&path, &index).unwrap();
        let loaded = load(&path, &[PathBuf::from("c:\\자료\\")]).unwrap();
        assert_eq!(loaded.entry(0).name, "한글.txt");
        assert_eq!(loaded.entry(0).created, Some(-1));
        assert_eq!(loaded.entry(0).size, Some(42));
        assert_eq!(loaded.skipped, 2);
        assert!(load(&path, &[PathBuf::from("D:\\")]).is_err());
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn rejects_truncated_and_oversized_lengths_without_allocating_them() {
        let path = temp();
        save(&path, &fixture()).unwrap();
        let original = fs::read(&path).unwrap();
        for length in [0, 20, original.len() - 1] {
            fs::write(&path, &original[..length]).unwrap();
            assert!(load(&path, &[PathBuf::from("C:\\자료")]).is_err());
        }
        let mut corrupt = original;
        // roots count follows magic/version and the three eight-byte stats.
        corrupt[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, corrupt).unwrap();
        assert!(load(&path, &[PathBuf::from("C:\\자료")]).is_err());
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn v2_cache_migrates_and_exclusion_removal_requires_sync() {
        let path = temp();
        let index = fixture();
        let roots = [PathBuf::from("C:\\자료")];
        save(&path, &index).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        let excluded_offset = 40 + index.roots.iter().map(|root| 4 + root.len()).sum::<usize>();
        bytes.drain(excluded_offset..excluded_offset + 4);
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        let loaded = load(&path, &roots).unwrap();
        assert_eq!(loaded.entry(0).name, "한글.txt");
        assert!(loaded.excluded_folders.is_empty());
        let excluded = [PathBuf::from("C:\\자료")];
        let filtered = load_with_exclusions(&path, &roots, &excluded).unwrap();
        assert!(filtered.is_empty());
        save(&path, &filtered).unwrap();
        assert!(
            load_with_exclusions(&path, &roots, &excluded)
                .unwrap()
                .is_empty()
        );
        assert!(load_with_exclusions(&path, &roots, &[]).is_err());
        assert!(load_with_exclusions(&path, &roots, &[PathBuf::from("C:\\자료\\하위")]).is_err());
        assert!(
            load_with_exclusions(&path, &roots, &[PathBuf::from("C:\\")])
                .unwrap()
                .is_empty()
        );
        let mut corrupt = fs::read(&path).unwrap();
        corrupt[excluded_offset..excluded_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        fs::write(&path, corrupt).unwrap();
        assert!(load(&path, &roots).is_err());
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn migrates_legacy_v1_without_losing_metadata() {
        let path = temp();
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&10u64.to_le_bytes());
        bytes.extend_from_slice(&2u64.to_le_bytes());
        bytes.extend_from_slice(&100i64.to_le_bytes());
        write_strings(&mut bytes, &["C:\\資料".into()]).unwrap();
        write_strings(&mut bytes, &["C:\\資料".into()]).unwrap();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        write_string(&mut bytes, "파일.PDF").unwrap();
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(7);
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        bytes.extend_from_slice(&(-123456i64).to_le_bytes());
        bytes.extend_from_slice(&9_999_999_999i64.to_le_bytes());
        fs::write(&path, bytes).unwrap();
        let index = load(&path, &[PathBuf::from("C:\\資料")]).unwrap();
        assert_eq!(index.entry(0).name, "파일.PDF");
        assert_eq!(index.entry(0).size, Some(u64::MAX));
        assert_eq!(index.entry(0).created, Some(-123456));
        assert_eq!(index.entry(0).modified, Some(9_999_999_999));
        save(&path, &index).unwrap();
        let restored = load(&path, &[PathBuf::from("C:\\資料")]).unwrap();
        assert_eq!(restored.entry(0).created, index.entry(0).created);
        assert_eq!(restored.entry(0).modified, index.entry(0).modified);
        fs::remove_file(path).unwrap();
    }
}
