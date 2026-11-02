//! Read-only NTFS indexing for the elevated helper.
//!
//! File names, parent links, logical lengths and timestamps come from the MFT,
//! in large sequential reads. USN is used only as a change log, never as a file
//! timestamp. We never create/reset a journal or lock/write to a volume.
use crate::{
    indexer::ScanProgress,
    model::{EntryView, Index},
};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const FRN_MASK: u64 = (1 << 48) - 1;
const MISSING_TIME: i64 = i64::MIN;
const MISSING_SIZE: u64 = u64::MAX;
const NODE_DIRECTORY: u16 = 1;
const NODE_REPARSE: u16 = 2;

#[derive(Clone, Copy, Debug)]
struct Node {
    frn: u64,
    parent: u64,
    name: u32,
    name_len: u16,
    flags: u16,
    size: u64,
    created: i64,
    modified: i64,
}

#[derive(Default)]
struct Snapshot {
    drive: u8,
    serial: u64,
    journal_id: u64,
    next_usn: i64,
    nodes: Vec<Node>,
    names: Vec<u8>,
    skipped: u64,
}
impl Snapshot {
    fn name(&self, node: &Node) -> &str {
        std::str::from_utf8(
            &self.names[node.name as usize..node.name as usize + node.name_len as usize],
        )
        .unwrap()
    }
    fn add(&mut self, parsed: &Parsed) -> Result<(), String> {
        for name in &parsed.names {
            if name.namespace == 2
                || (name.name == "." && parsed.frn & FRN_MASK != 5)
                || name.name.is_empty()
            {
                continue;
            }
            let offset = u32::try_from(self.names.len())
                .map_err(|_| "NTFS 파일명 풀이 지원 한도를 초과했습니다.")?;
            let len = u16::try_from(name.name.len()).map_err(|_| "NTFS 파일명이 너무 깁니다.")?;
            self.names.extend_from_slice(name.name.as_bytes());
            self.nodes.push(Node {
                frn: parsed.frn,
                parent: name.parent,
                name: offset,
                name_len: len,
                flags: (if parsed.is_dir { NODE_DIRECTORY } else { 0 })
                    | (if parsed.is_reparse { NODE_REPARSE } else { 0 }),
                size: parsed.size.unwrap_or(MISSING_SIZE),
                created: parsed.created.unwrap_or(MISSING_TIME),
                modified: parsed.modified.unwrap_or(MISSING_TIME),
            });
        }
        Ok(())
    }
}

#[derive(Default, Debug)]
struct Parsed {
    frn: u64,
    base: u64,
    is_dir: bool,
    is_reparse: bool,
    created: Option<i64>,
    modified: Option<i64>,
    size: Option<u64>,
    names: Vec<FileName>,
}
#[derive(Debug)]
struct FileName {
    parent: u64,
    namespace: u8,
    name: String,
}
#[derive(Clone, Copy, Debug)]
struct Run {
    vcn: u64,
    lcn: u64,
    clusters: u64,
}

/// Preserve base metadata even when all its names live in extension records.
/// Resolve all DATA attributes before adding extension names, so their result
/// cannot depend on the order in which extension segments happen to appear.
fn merge_extensions(
    snapshot: &mut Snapshot,
    mut nameless_bases: std::collections::HashMap<u64, Parsed>,
    extensions: Vec<Parsed>,
) -> Result<(), String> {
    use std::collections::HashSet;
    for extension in &extensions {
        if let Some(size) = extension.size {
            let found = snapshot
                .nodes
                .partition_point(|n| (n.frn & FRN_MASK) < (extension.base & FRN_MASK));
            let mut cursor = found;
            while let Some(node) = snapshot
                .nodes
                .get_mut(cursor)
                .filter(|n| n.frn == extension.base)
            {
                node.size = size;
                cursor += 1;
            }
            if let Some(base) = nameless_bases.get_mut(&extension.base) {
                base.size = Some(size);
            }
        }
    }
    let mut additions = Vec::new();
    let mut has_long_names = HashSet::new();
    for mut extension in extensions {
        let base = extension.base;
        let found = snapshot
            .nodes
            .partition_point(|n| (n.frn & FRN_MASK) < (base & FRN_MASK));
        if let Some(meta) = snapshot.nodes.get(found).filter(|n| n.frn == base) {
            extension.is_dir = meta.flags & NODE_DIRECTORY != 0;
            extension.is_reparse = meta.flags & NODE_REPARSE != 0;
            extension.created = (meta.created != MISSING_TIME).then_some(meta.created);
            extension.modified = (meta.modified != MISSING_TIME).then_some(meta.modified);
            extension.size = (meta.size != MISSING_SIZE).then_some(meta.size);
        } else if let Some(meta) = nameless_bases.get(&base) {
            extension.is_dir = meta.is_dir;
            extension.is_reparse = meta.is_reparse;
            extension.created = meta.created;
            extension.modified = meta.modified;
            extension.size = meta.size;
        } else {
            continue;
        }
        extension.frn = base;
        extension.base = 0;
        if extension.names.iter().any(|name| name.namespace != 2) {
            has_long_names.insert(base);
        }
        additions.push(extension);
    }
    for extension in additions {
        snapshot.add(&extension)?;
    }
    // A DOS-only base is a valid searchable entry if there is no long-name
    // extension. Do not expose its alias when a long name was found.
    for (frn, mut base) in nameless_bases.drain() {
        if !has_long_names.contains(&frn) && !base.names.is_empty() {
            for name in &mut base.names {
                if name.namespace == 2 {
                    name.namespace = 3;
                }
            }
            snapshot.add(&base)?;
        }
    }
    snapshot.nodes.sort_unstable_by_key(|n| n.frn & FRN_MASK);
    Ok(())
}

fn affected_parent_references(
    snapshot: &Snapshot,
    affected_numbers: &std::collections::HashSet<u64>,
    refreshed: &[Parsed],
) -> std::collections::HashSet<u64> {
    let mut parents = std::collections::HashSet::new();
    for node in &snapshot.nodes {
        if affected_numbers.contains(&(node.frn & FRN_MASK)) {
            parents.insert(node.parent);
        }
    }
    for parsed in refreshed {
        for name in &parsed.names {
            parents.insert(name.parent);
        }
    }
    parents
}

fn u16_at(b: &[u8], p: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(p..p + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], p: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(p..p + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], p: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(p..p + 8)?.try_into().ok()?))
}
fn i64_at(b: &[u8], p: usize) -> Option<i64> {
    Some(i64::from_le_bytes(b.get(p..p + 8)?.try_into().ok()?))
}
fn filetime(ticks: u64) -> Option<i64> {
    (ticks != 0).then_some((ticks / 10_000_000) as i64 - 11_644_473_600)
}

/// Restore and validate every 512-byte update-sequence trailer before parsing.
/// A torn/concurrently-written record is retried through the filesystem IOCTL.
fn fixup(record: &mut [u8]) -> Result<(), String> {
    if record.get(..4) != Some(b"FILE") {
        return Err("NTFS FILE 서명이 없습니다.".into());
    }
    let start = u16_at(record, 4).ok_or("잘린 NTFS 헤더")? as usize;
    let count = u16_at(record, 6).ok_or("잘린 NTFS 헤더")? as usize;
    if count != record.len() / 512 + 1 || start < 8 || start + count * 2 > 510 {
        return Err("NTFS 업데이트 시퀀스 범위가 올바르지 않습니다.".into());
    }
    let sequence = u16_at(record, start).ok_or("잘린 NTFS 시퀀스")?;
    for sector in 1..count {
        let end = sector * 512 - 2;
        if u16_at(record, end) != Some(sequence) {
            return Err("동시에 변경된 NTFS 레코드입니다.".into());
        }
    }
    for sector in 1..count {
        let end = sector * 512 - 2;
        let word = u16_at(record, start + sector * 2).unwrap().to_le_bytes();
        record[end..end + 2].copy_from_slice(&word);
    }
    Ok(())
}

/// FSCTL_GET_NTFS_FILE_RECORD supplies the filesystem's logical record, whose
/// cached contents can be newer than its saved on-disk USA trailer words. Only
/// apply disk fixups when *all* trailers carry the tag; otherwise validate the
/// actual logical header/attributes rather than comparing stale saved words.
fn normalize_ioctl_record(record: &mut [u8]) -> Result<(), String> {
    if record.get(..4) != Some(b"FILE") {
        return Err("NTFS FILE 서명이 없습니다.".into());
    }
    let start = u16_at(record, 4).ok_or("잘린 NTFS 헤더")? as usize;
    let count = u16_at(record, 6).ok_or("잘린 NTFS 헤더")? as usize;
    if record.len() < 512
        || record.len() % 512 != 0
        || count != record.len() / 512 + 1
        || start < 8
        || start + count * 2 > 510
    {
        return Err("NTFS 업데이트 시퀀스 범위가 올바르지 않습니다.".into());
    }
    let tag = u16_at(record, start).unwrap();
    if (1..count).all(|sector| u16_at(record, sector * 512 - 2) == Some(tag)) {
        fixup(record)?;
    }
    let allocated = u32_at(record, 28).ok_or("잘린 NTFS 파일 크기")? as usize;
    if allocated != record.len() {
        return Err("NTFS 파일 레코드 할당 크기가 올바르지 않습니다.".into());
    }
    let first = u16_at(record, 20).ok_or("잘린 NTFS 속성 헤더")? as usize;
    if first < start + count * 2 || first % 8 != 0 {
        return Err("NTFS 속성 시작 범위가 올바르지 않습니다.".into());
    }
    for attribute in attributes(record)? {
        let minimum = match attribute[8] {
            0 => 24,
            1 => 64,
            _ => return Err("NTFS 속성 형식이 올바르지 않습니다.".into()),
        };
        if attribute.len() < minimum {
            return Err("잘린 NTFS 속성 헤더입니다.".into());
        }
        let name_len = attribute[9] as usize * 2;
        let name_start = u16_at(attribute, 10).unwrap() as usize;
        if name_len > 0 && (name_start < minimum || name_start + name_len > attribute.len()) {
            return Err("NTFS 속성 이름 범위가 올바르지 않습니다.".into());
        }
        if attribute[8] == 0 {
            let value_len = u32_at(attribute, 16).unwrap() as usize;
            let value_start = u16_at(attribute, 20).unwrap() as usize;
            if value_start < minimum
                || value_start
                    .checked_add(value_len)
                    .is_none_or(|end| end > attribute.len())
            {
                return Err("NTFS 상주 속성 범위가 올바르지 않습니다.".into());
            }
        } else {
            let run_start = u16_at(attribute, 32).unwrap() as usize;
            if run_start < minimum || run_start >= attribute.len() {
                return Err("NTFS 비상주 속성 범위가 올바르지 않습니다.".into());
            }
        }
    }
    Ok(())
}

fn attributes(record: &[u8]) -> Result<Vec<&[u8]>, String> {
    let mut position = u16_at(record, 20).ok_or("잘린 NTFS 속성 헤더")? as usize;
    let used = u32_at(record, 24).ok_or("잘린 NTFS 크기")? as usize;
    if position < 42 || used > record.len() || position > used {
        return Err("NTFS 속성 범위 오류".into());
    }
    let mut found = Vec::new();
    while position + 4 <= used {
        if u32_at(record, position) == Some(u32::MAX) {
            return Ok(found);
        }
        let len = u32_at(record, position + 4).ok_or("잘린 NTFS 속성")? as usize;
        if len < 24 || len % 8 != 0 || len > used - position {
            return Err("NTFS 속성 길이 오류".into());
        }
        found.push(&record[position..position + len]);
        position += len;
    }
    Err("NTFS 속성 종료 표시가 없습니다.".into())
}
fn resident(attribute: &[u8]) -> Option<&[u8]> {
    if *attribute.get(8)? != 0 {
        return None;
    }
    let size = u32_at(attribute, 16)? as usize;
    let start = u16_at(attribute, 20)? as usize;
    if start < 24 {
        return None;
    }
    attribute.get(start..start.checked_add(size)?)
}

fn parse_record(record: &[u8], number: u64) -> Result<Option<Parsed>, String> {
    let flags = u16_at(record, 22).ok_or("잘린 NTFS 파일 헤더")?;
    if flags & 1 == 0 {
        return Ok(None);
    }
    let sequence = u16_at(record, 16).ok_or("잘린 NTFS 순서번호")?;
    let mut out = Parsed {
        frn: number | (u64::from(sequence) << 48),
        base: u64_at(record, 32).ok_or("잘린 NTFS 기본 레코드")?,
        is_dir: flags & 2 != 0,
        ..Parsed::default()
    };
    for attribute in attributes(record)? {
        match u32_at(attribute, 0).unwrap() {
            0x10 => {
                if let Some(value) = resident(attribute).filter(|v| v.len() >= 36) {
                    out.created = filetime(u64_at(value, 0).unwrap());
                    out.modified = filetime(u64_at(value, 8).unwrap());
                    out.is_reparse = u32_at(value, 32).unwrap() & 0x400 != 0;
                }
            }
            0x30 => {
                if let Some(value) = resident(attribute).filter(|v| v.len() >= 66) {
                    let chars = value[64] as usize;
                    let bytes = value.get(66..66 + chars * 2).ok_or("잘린 NTFS 파일명")?;
                    let wide: Vec<u16> = bytes
                        .chunks_exact(2)
                        .map(|p| u16::from_le_bytes([p[0], p[1]]))
                        .collect();
                    let name = String::from_utf16_lossy(&wide);
                    if name.contains(['\\', '/', '\0']) {
                        return Err("NTFS 파일명에 경로 구분자가 있습니다.".into());
                    }
                    out.names.push(FileName {
                        parent: u64_at(value, 0).unwrap(),
                        namespace: value[65],
                        name,
                    });
                }
            }
            0x80 if attribute[9] == 0 => {
                if attribute[8] == 0 {
                    out.size = resident(attribute).map(|v| v.len() as u64);
                } else if u64_at(attribute, 16) == Some(0) {
                    out.size = u64_at(attribute, 48);
                }
            }
            _ => {}
        }
    }
    // A genuinely empty file has a resident zero-length unnamed $DATA. If its
    // $DATA lives in an extension, metadata is merged after the sequential pass.
    Ok(Some(out))
}

fn parse_runs(attribute: &[u8]) -> Result<Vec<Run>, String> {
    if attribute.get(8) != Some(&1) {
        return Err("NTFS 비상주 데이터 속성이 아닙니다.".into());
    }
    let mut vcn = u64_at(attribute, 16).ok_or("잘린 NTFS VCN")?;
    let mut pos = u16_at(attribute, 32).ok_or("잘린 NTFS 런 목록")? as usize;
    let mut lcn = 0_i64;
    let mut runs = Vec::new();
    loop {
        let head = *attribute.get(pos).ok_or("잘린 NTFS 런 목록")?;
        pos += 1;
        if head == 0 {
            break;
        }
        let count_bytes = (head & 15) as usize;
        let offset_bytes = (head >> 4) as usize;
        if count_bytes == 0
            || count_bytes > 8
            || offset_bytes == 0
            || offset_bytes > 8
            || pos + count_bytes + offset_bytes > attribute.len()
        {
            return Err("지원하지 않는 NTFS 희소/손상 런 목록입니다.".into());
        }
        let mut n = [0_u8; 8];
        n[..count_bytes].copy_from_slice(&attribute[pos..pos + count_bytes]);
        let clusters = u64::from_le_bytes(n);
        pos += count_bytes;
        let sign = attribute[pos + offset_bytes - 1] & 0x80 != 0;
        let mut delta = if sign { [0xff; 8] } else { [0; 8] };
        delta[..offset_bytes].copy_from_slice(&attribute[pos..pos + offset_bytes]);
        pos += offset_bytes;
        lcn = lcn
            .checked_add(i64::from_le_bytes(delta))
            .ok_or("NTFS LCN 오버플로")?;
        if lcn < 0 || clusters == 0 {
            return Err("올바르지 않은 NTFS 런입니다.".into());
        }
        runs.push(Run {
            vcn,
            lcn: lcn as u64,
            clusters,
        });
        vcn = vcn.checked_add(clusters).ok_or("NTFS VCN 오버플로")?;
    }
    Ok(runs)
}

#[derive(serde::Serialize)]
pub struct FastReport {
    pub entries: usize,
    pub elapsed_ms: u64,
    pub replayed_volumes: usize,
    pub rebuilt_volumes: usize,
    pub changed_records: usize,
    pub hydrated_records: usize,
    pub source: &'static str,
    pub timings: FastTimings,
}
#[derive(Default, serde::Serialize)]
pub struct FastTimings {
    pub load_ms: f64,
    pub replay_ms: f64,
    pub hydrate_ms: f64,
    pub build_ms: f64,
    pub sidecar_save_ms: f64,
    pub cache_save_ms: f64,
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::{
        collections::{HashMap, HashSet},
        ffi::OsStr,
        mem::{size_of, zeroed},
        os::windows::ffi::OsStrExt,
        ptr::{null, null_mut},
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GENERIC_READ, GetLastError, HANDLE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, FILE_BEGIN, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING, ReadFile, SetFilePointerEx,
        },
        System::{
            IO::DeviceIoControl,
            Ioctl::{
                FSCTL_GET_NTFS_FILE_RECORD, FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL,
                FSCTL_READ_USN_JOURNAL, NTFS_VOLUME_DATA_BUFFER, READ_USN_JOURNAL_DATA_V0,
            },
        },
    };

    struct Volume {
        handle: HANDLE,
        data: NTFS_VOLUME_DATA_BUFFER,
        drive: u8,
    }
    impl Drop for Volume {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
    fn os_error(context: &str) -> String {
        format!(
            "{context}: {}",
            std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
        )
    }
    impl Volume {
        fn open(drive: u8) -> Result<Self, String> {
            let device = format!(r"\\.\{}:", drive as char);
            let wide: Vec<u16> = OsStr::new(&device).encode_wide().chain(Some(0)).collect();
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    null(),
                    OPEN_EXISTING,
                    0,
                    null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(os_error("NTFS 볼륨 열기 실패 (관리자 권한 필요)"));
            }
            let mut data: NTFS_VOLUME_DATA_BUFFER = unsafe { zeroed() };
            let mut returned = 0;
            let ok = unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_GET_NTFS_VOLUME_DATA,
                    null(),
                    0,
                    (&mut data as *mut NTFS_VOLUME_DATA_BUFFER).cast(),
                    size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
                    &mut returned,
                    null_mut(),
                )
            };
            if ok == 0 {
                unsafe { CloseHandle(handle) };
                return Err(os_error("NTFS 볼륨 정보 읽기 실패"));
            }
            let record = data.BytesPerFileRecordSegment as usize;
            if !(512..=65536).contains(&record)
                || !record.is_power_of_two()
                || data.BytesPerCluster == 0
                || data.MftValidDataLength <= 0
            {
                unsafe { CloseHandle(handle) };
                return Err("지원하지 않는 NTFS 볼륨 형식입니다.".into());
            }
            Ok(Self {
                handle,
                data,
                drive,
            })
        }
        fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize, String> {
            let mut returned = 0;
            let ok = unsafe {
                DeviceIoControl(
                    self.handle,
                    code,
                    if input.is_empty() {
                        null()
                    } else {
                        input.as_ptr().cast()
                    },
                    input.len() as u32,
                    output.as_mut_ptr().cast(),
                    output.len() as u32,
                    &mut returned,
                    null_mut(),
                )
            };
            if ok == 0 {
                return Err(os_error("NTFS 제어 요청 실패"));
            }
            Ok(returned as usize)
        }
        fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), String> {
            if offset > i64::MAX as u64 {
                return Err("NTFS 읽기 위치 오버플로".into());
            }
            if unsafe { SetFilePointerEx(self.handle, offset as i64, null_mut(), FILE_BEGIN) } == 0
            {
                return Err(os_error("NTFS 읽기 위치 설정 실패"));
            }
            let mut done = 0;
            while done < buffer.len() {
                let mut read = 0;
                if unsafe {
                    ReadFile(
                        self.handle,
                        buffer[done..].as_mut_ptr(),
                        (buffer.len() - done).min(u32::MAX as usize) as u32,
                        &mut read,
                        null_mut(),
                    )
                } == 0
                {
                    return Err(os_error("NTFS MFT 읽기 실패"));
                }
                if read == 0 {
                    return Err("NTFS MFT가 예기치 않게 끝났습니다.".into());
                }
                done += read as usize;
            }
            Ok(())
        }
        fn record_if_exists(&self, frn: u64) -> Result<Option<Vec<u8>>, String> {
            let number = frn & FRN_MASK;
            let mut out = vec![0; self.data.BytesPerFileRecordSegment as usize + 16];
            let mut last_error = String::new();
            for _ in 0..3 {
                let len =
                    self.ioctl(FSCTL_GET_NTFS_FILE_RECORD, &number.to_le_bytes(), &mut out)?;
                if len < 12 {
                    return Err("잘린 NTFS 파일 응답입니다.".into());
                }
                if u64_at(&out, 0).map(|x| x & FRN_MASK) != Some(number) {
                    return Ok(None);
                }
                let bytes = u32_at(&out, 8).ok_or("잘린 NTFS 파일 응답")? as usize;
                let mut record = out
                    .get(12..12 + bytes)
                    .filter(|_| 12 + bytes <= len)
                    .ok_or("잘린 NTFS 파일 응답")?
                    .to_vec();
                match normalize_ioctl_record(&mut record) {
                    Ok(()) => return Ok(Some(record)),
                    Err(error) => last_error = error,
                }
            }
            Err(format!(
                "NTFS 파일 레코드 {number} 읽기 실패 (3회 확인): {last_error}"
            ))
        }
        fn file_record(&self, frn: u64) -> Result<Vec<u8>, String> {
            self.record_if_exists(frn)?
                .ok_or_else(|| "요청한 NTFS 파일 레코드가 더 이상 없습니다.".into())
        }
        fn attribute_list(&self, attribute: &[u8]) -> Result<Vec<u8>, String> {
            if let Some(value) = resident(attribute) {
                return Ok(value.to_vec());
            }
            let size = u64_at(attribute, 48).ok_or("잘린 NTFS 속성 목록")?;
            if size > 16 * 1024 * 1024 {
                return Err("NTFS 속성 목록이 너무 큽니다.".into());
            }
            let mut value = Vec::with_capacity(size as usize);
            for run in parse_runs(attribute)? {
                let bytes = run
                    .clusters
                    .checked_mul(self.data.BytesPerCluster as u64)
                    .ok_or("NTFS 속성 목록 크기 오버플로")?;
                if bytes > 16 * 1024 * 1024 {
                    return Err("NTFS 속성 목록이 너무 큽니다.".into());
                }
                let mut b = vec![0; bytes as usize];
                self.read_at(run.lcn * self.data.BytesPerCluster as u64, &mut b)?;
                value.extend_from_slice(&b);
            }
            if value.len() < size as usize {
                return Err("잘린 NTFS 속성 목록입니다.".into());
            }
            value.truncate(size as usize);
            Ok(value)
        }
        fn complete_record(&self, number: u64) -> Result<Option<Parsed>, String> {
            let Some(mut record) = self.record_if_exists(number)? else {
                return Ok(None);
            };
            let Some(mut parsed) = parse_record(&record, number & FRN_MASK)? else {
                return Ok(None);
            };
            if parsed.base != 0 {
                record = self.file_record(parsed.base)?;
                parsed = parse_record(&record, parsed.base & FRN_MASK)?
                    .ok_or("NTFS 기본 레코드가 사라졌습니다.")?;
            }
            let mut ids = HashSet::new();
            for attr in attributes(&record)? {
                if u32_at(attr, 0) != Some(0x20) {
                    continue;
                }
                let list = self.attribute_list(attr)?;
                let mut p = 0;
                while p + 26 <= list.len() {
                    let len = u16_at(&list, p + 4).unwrap() as usize;
                    if len < 26 || len > list.len() - p {
                        return Err("손상된 NTFS 속성 목록입니다.".into());
                    }
                    let frn = u64_at(&list, p + 16).unwrap();
                    if frn != parsed.frn {
                        ids.insert(frn);
                    }
                    p += len;
                }
            }
            for frn in ids {
                let record = self.file_record(frn)?;
                if let Some(mut extra) = parse_record(&record, frn & FRN_MASK)? {
                    if extra.base != parsed.frn {
                        return Err("NTFS 확장 레코드 참조가 변경되었습니다.".into());
                    }
                    parsed.names.append(&mut extra.names);
                    if extra.size.is_some() {
                        parsed.size = extra.size;
                    }
                }
            }
            Ok(Some(parsed))
        }
        fn journal(&self) -> Option<(u64, i64, i64)> {
            let mut b = [0; 128];
            let len = self.ioctl(FSCTL_QUERY_USN_JOURNAL, &[], &mut b).ok()?;
            if len < 56 {
                return None;
            }
            Some((
                u64_at(&b, 0)?,
                i64_at(&b, 16)?,
                i64_at(&b, 24)?.max(i64_at(&b, 8)?),
            ))
        }
        fn changed(&self, snapshot: &Snapshot) -> Result<(HashSet<u64>, i64), String> {
            let (id, target, lowest) = self.journal().ok_or("USN 변경저널을 읽을 수 없습니다.")?;
            if snapshot.journal_id == 0
                || id != snapshot.journal_id
                || snapshot.next_usn < lowest
                || snapshot.next_usn > target
            {
                return Err("USN 저널이 변경되거나 보존 범위를 벗어났습니다.".into());
            }
            let mut next = snapshot.next_usn;
            let mut changes = HashSet::new();
            let mut buffer = vec![0; 1024 * 1024];
            while next < target {
                let input = READ_USN_JOURNAL_DATA_V0 {
                    StartUsn: next,
                    ReasonMask: u32::MAX,
                    ReturnOnlyOnClose: 0,
                    Timeout: 0,
                    BytesToWaitFor: 0,
                    UsnJournalID: id,
                };
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        (&input as *const READ_USN_JOURNAL_DATA_V0).cast::<u8>(),
                        size_of::<READ_USN_JOURNAL_DATA_V0>(),
                    )
                };
                let len = self.ioctl(FSCTL_READ_USN_JOURNAL, bytes, &mut buffer)?;
                if len < 8 {
                    return Err("잘린 USN 변경 응답입니다.".into());
                }
                let cursor = i64_at(&buffer, 0).unwrap();
                if cursor <= next {
                    return Err("USN 변경 읽기가 진행되지 않습니다.".into());
                }
                let mut pos = 8;
                while pos < len {
                    let record_len = u32_at(&buffer, pos).ok_or("잘린 USN 레코드")? as usize;
                    if record_len < 60
                        || record_len > len - pos
                        || u16_at(&buffer, pos + 4) != Some(2)
                    {
                        return Err("지원하지 않는 USN 레코드 형식입니다.".into());
                    }
                    // Beyond the captured cursor will be handled on the next run.
                    if i64_at(&buffer, pos + 24).unwrap() < target {
                        changes.insert(u64_at(&buffer, pos + 8).unwrap());
                    }
                    pos += record_len;
                }
                next = cursor;
                if changes.len() > 500_000 {
                    return Err("USN 변경량이 많아 MFT를 다시 읽습니다.".into());
                }
            }
            Ok((changes, target))
        }
        fn runs(&self) -> Result<Vec<Run>, String> {
            let base = self.file_record(0)?;
            let mut runs = Vec::new();
            let mut extension_ids = HashSet::new();
            let mut attribute_list = Vec::new();
            for attr in attributes(&base)? {
                if u32_at(attr, 0) == Some(0x80) && attr[9] == 0 {
                    runs.extend(parse_runs(attr)?);
                }
                if u32_at(attr, 0) == Some(0x20) {
                    if let Some(value) = resident(attr) {
                        attribute_list.extend_from_slice(value);
                    } else {
                        let size = u64_at(attr, 48).ok_or("잘린 MFT 속성 목록")?;
                        if size > 16 * 1024 * 1024 {
                            return Err("MFT 속성 목록이 너무 큽니다.".into());
                        }
                        let clusters = self.data.BytesPerCluster as usize;
                        for run in parse_runs(attr)? {
                            let mut b =
                                vec![0; (run.clusters as usize * clusters).min(16 * 1024 * 1024)];
                            self.read_at(run.lcn * clusters as u64, &mut b)?;
                            attribute_list.extend_from_slice(&b);
                        }
                        attribute_list.truncate(size as usize);
                    }
                }
            }
            let mut p = 0;
            while p + 26 <= attribute_list.len() {
                let kind = u32_at(&attribute_list, p).unwrap();
                if kind == u32::MAX || kind == 0 {
                    break;
                }
                let len = u16_at(&attribute_list, p + 4).unwrap() as usize;
                if len < 26 || len > attribute_list.len() - p {
                    return Err("손상된 MFT 속성 목록입니다.".into());
                }
                if kind == 0x80 && attribute_list[p + 6] == 0 {
                    let frn = u64_at(&attribute_list, p + 16).unwrap();
                    if frn & FRN_MASK != 0 {
                        extension_ids.insert(frn);
                    }
                }
                p += len;
            }
            for frn in extension_ids {
                let extension = self.file_record(frn)?;
                for attr in attributes(&extension)? {
                    if u32_at(attr, 0) == Some(0x80) && attr[9] == 0 {
                        runs.extend(parse_runs(attr)?);
                    }
                }
            }
            runs.sort_unstable_by_key(|r| r.vcn);
            runs.dedup_by_key(|r| r.vcn);
            let mut end = 0;
            for run in &runs {
                if run.vcn != end {
                    return Err("MFT 데이터 런이 연속되지 않습니다.".into());
                }
                end = run
                    .vcn
                    .checked_add(run.clusters)
                    .ok_or("MFT 데이터 런 오버플로")?;
            }
            if end.saturating_mul(self.data.BytesPerCluster as u64)
                < self.data.MftValidDataLength as u64
            {
                return Err("MFT 데이터 런 목록이 불완전합니다.".into());
            }
            Ok(runs)
        }
        fn scan(
            &self,
            cancel: &AtomicBool,
            progress: &impl Fn(ScanProgress),
            started: Instant,
        ) -> Result<Snapshot, String> {
            let captured = self.journal();
            let mut snapshot = Snapshot {
                drive: self.drive,
                serial: self.data.VolumeSerialNumber as u64,
                journal_id: captured.map(|j| j.0).unwrap_or(0),
                next_usn: captured.map(|j| j.1).unwrap_or(0),
                ..Snapshot::default()
            };
            let record_size = self.data.BytesPerFileRecordSegment as usize;
            let cluster = self.data.BytesPerCluster as u64;
            let total = self.data.MftValidDataLength as u64;
            // The MFT includes unused slots. Reserve conservatively, bounded by
            // its valid byte length; the helper releases all buffers on exit.
            snapshot
                .nodes
                .reserve((total / record_size as u64).min(5_000_000) as usize / 2);
            let mut extensions = Vec::new();
            let mut nameless_bases = HashMap::new();
            let mut buffer = vec![0; 4 * 1024 * 1024];
            let mut last = Instant::now();
            for run in self.runs()? {
                let mut offset = run.vcn * cluster;
                let end = ((run.vcn + run.clusters) * cluster).min(total);
                while offset < end {
                    if cancel.load(Ordering::Relaxed) {
                        return Err("인덱싱을 취소했습니다.".into());
                    }
                    let size = (end - offset).min(buffer.len() as u64) as usize;
                    self.read_at(
                        run.lcn * cluster + (offset - run.vcn * cluster),
                        &mut buffer[..size],
                    )?;
                    for (i, record) in buffer[..size].chunks_exact_mut(record_size).enumerate() {
                        let number = offset / record_size as u64 + i as u64;
                        if record.get(..4) != Some(b"FILE") {
                            continue;
                        }
                        let parsed = if fixup(record).is_ok() {
                            parse_record(record, number)
                        } else {
                            self.file_record(number)
                                .and_then(|b| parse_record(&b, number))
                        };
                        match parsed {
                            Ok(Some(parsed)) if parsed.base != 0 => extensions.push(parsed),
                            Ok(Some(parsed)) => {
                                if parsed.names.iter().any(|n| n.namespace != 2) {
                                    snapshot.add(&parsed)?;
                                } else {
                                    nameless_bases.insert(parsed.frn, parsed);
                                }
                            }
                            Ok(None) => {}
                            Err(_) => snapshot.skipped += 1,
                        }
                    }
                    offset += size as u64;
                    if last.elapsed().as_millis() >= 150 {
                        progress(ScanProgress {
                            entries: snapshot.nodes.len(),
                            folders: 0,
                            skipped: snapshot.skipped,
                            elapsed_ms: started.elapsed().as_millis() as u64,
                        });
                        last = Instant::now();
                    }
                }
            }
            merge_extensions(&mut snapshot, nameless_bases, extensions)?;
            // Capture-before-scan plus replay closes the concurrent scan window.
            if snapshot.journal_id != 0 {
                self.replay(&mut snapshot, cancel)?;
            }
            snapshot.nodes.shrink_to_fit();
            snapshot.names.shrink_to_fit();
            Ok(snapshot)
        }
        fn replay(&self, snapshot: &mut Snapshot, cancel: &AtomicBool) -> Result<usize, String> {
            let (changes, next) = self.changed(snapshot)?;
            if changes.is_empty() {
                snapshot.next_usn = next;
                return Ok(0);
            }
            let numbers: HashSet<u64> = changes.iter().map(|f| f & FRN_MASK).collect();
            // Query current records first: a deleted slot may have been reused.
            // Old names are removed only after every metadata query succeeds or
            // proves the slot gone, rather than on an ambiguous IOCTL failure.
            let mut refreshed = Vec::new();
            let mut refreshed_ids = HashSet::new();
            for number in &numbers {
                if cancel.load(Ordering::Relaxed) {
                    return Err("인덱싱을 취소했습니다.".into());
                }
                if let Some(parsed) = self.complete_record(*number)? {
                    if refreshed_ids.insert(parsed.frn) {
                        refreshed.push(parsed);
                    }
                }
            }
            let mut affected = numbers;
            for parsed in &refreshed {
                affected.insert(parsed.frn & FRN_MASK);
            }
            // NTFS need not emit a directory USN record when a child changes
            // its membership and therefore its last-write time. Refresh both
            // sides of moves, plus parents of deleted records, before advancing
            // the checkpoint; refreshing only child FRNs leaves stale dates.
            let parents = affected_parent_references(snapshot, &affected, &refreshed);
            for parent in parents {
                if cancel.load(Ordering::Relaxed) {
                    return Err("인덱싱을 취소했습니다.".into());
                }
                if refreshed_ids.contains(&parent) {
                    continue;
                }
                if let Some(parsed) = self.complete_record(parent)? {
                    if parsed.frn == parent && parsed.is_dir && refreshed_ids.insert(parent) {
                        affected.insert(parent & FRN_MASK);
                        refreshed.push(parsed);
                    }
                }
            }
            snapshot
                .nodes
                .retain(|n| !affected.contains(&(n.frn & FRN_MASK)));
            for parsed in &refreshed {
                snapshot.add(parsed)?;
            }
            snapshot.nodes.sort_unstable_by_key(|n| n.frn & FRN_MASK);
            snapshot.next_usn = next;
            // Reclaim names belonging to removed/replaced records.
            let used: usize = snapshot.nodes.iter().map(|n| n.name_len as usize).sum();
            if snapshot.names.len() > used + 8 * 1024 * 1024 {
                let mut names = Vec::with_capacity(used);
                for node in &mut snapshot.nodes {
                    let old = node.name as usize;
                    node.name = names.len() as u32;
                    names.extend_from_slice(&snapshot.names[old..old + node.name_len as usize]);
                }
                snapshot.names = names;
            }
            Ok(changes.len())
        }

        /// Some base records put every $FILE_NAME, or their unnamed $DATA,
        /// into attribute-list extension records. Fill only these exceptions
        /// through NTFS itself instead of issuing millions of file queries.
        fn hydrate_incomplete(
            &self,
            snapshot: &mut Snapshot,
            cancel: &AtomicBool,
            progress: &impl Fn(ScanProgress),
            started: Instant,
        ) -> Result<usize, String> {
            let mut queried = HashSet::new();
            let mut hydrated = 0;
            for _ in 0..256 {
                let directories: HashSet<u64> = snapshot
                    .nodes
                    .iter()
                    .filter(|n| n.flags & NODE_DIRECTORY != 0)
                    .map(|n| n.frn)
                    .collect();
                let mut pending = HashSet::new();
                for node in &snapshot.nodes {
                    if !directories.contains(&node.parent) && !queried.contains(&node.parent) {
                        pending.insert(node.parent);
                    }
                    if node.flags & NODE_DIRECTORY == 0
                        && node.size == MISSING_SIZE
                        && !queried.contains(&node.frn)
                    {
                        pending.insert(node.frn);
                    }
                }
                if pending.is_empty() {
                    break;
                }
                let mut replacements = Vec::new();
                let mut replaced = HashSet::new();
                for frn in pending {
                    if cancel.load(Ordering::Relaxed) {
                        return Err("인덱싱을 취소했습니다.".into());
                    }
                    queried.insert(frn);
                    if let Some(parsed) = self.complete_record(frn)? {
                        // A reused slot must never reconnect an old subtree to
                        // an unrelated directory with the same record number.
                        if parsed.frn == frn && replaced.insert(frn) {
                            if !parsed.names.iter().any(|n| n.namespace != 2) {
                                return Err(
                                    "NTFS 확장 파일명 정보를 완전히 읽을 수 없습니다.".into()
                                );
                            }
                            replacements.push(parsed);
                        }
                    }
                    if queried.len() % 200 == 0 {
                        progress(ScanProgress {
                            entries: snapshot.nodes.len(),
                            folders: directories.len(),
                            skipped: snapshot.skipped,
                            elapsed_ms: started.elapsed().as_millis() as u64,
                        });
                    }
                }
                snapshot.nodes.retain(|n| !replaced.contains(&n.frn));
                for parsed in replacements {
                    snapshot.add(&parsed)?;
                    hydrated += 1;
                }
            }
            snapshot.nodes.sort_unstable_by_key(|n| n.frn & FRN_MASK);
            Ok(hydrated)
        }
    }

    fn normalized(path: &str) -> String {
        path.replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    }
    fn child(path: &str, ancestor: &str) -> bool {
        path == ancestor
            || path
                .strip_prefix(ancestor)
                .is_some_and(|s| s.starts_with('\\'))
    }
    fn roots_by_drive(roots: &[PathBuf]) -> Result<HashMap<u8, Vec<String>>, String> {
        let mut grouped: HashMap<u8, Vec<String>> = HashMap::new();
        for root in roots {
            let absolute = std::fs::canonicalize(root)
                .map_err(|e| format!("검색 폴더를 열 수 없습니다: {e}"))?;
            let text = absolute.to_string_lossy();
            let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
            let bytes = text.as_bytes();
            if bytes.len() < 3 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
                return Err("NTFS 고속 검색은 로컬 드라이브에서만 지원합니다.".into());
            }
            grouped
                .entry(bytes[0].to_ascii_uppercase())
                .or_default()
                .push(normalized(text));
        }
        if grouped.is_empty() {
            return Err("검색할 드라이브가 없습니다.".into());
        }
        Ok(grouped)
    }

    pub(super) fn append_index(
        index: &mut Index,
        snapshot: &Snapshot,
        roots: &[String],
    ) -> Result<(), String> {
        let root = format!("{}:\\", snapshot.drive as char);
        let whole_volume = roots.len() == 1 && roots[0] == normalized(&root);
        let mut directory_nodes: HashMap<u64, usize> = HashMap::new();
        for (i, node) in snapshot.nodes.iter().enumerate() {
            if node.flags & NODE_DIRECTORY != 0 {
                directory_nodes.entry(node.frn).or_insert(i);
            }
        }
        let root_ref = snapshot
            .nodes
            .iter()
            .find(|n| n.frn & FRN_MASK == 5)
            .map(|n| n.frn)
            .ok_or("NTFS 루트 디렉터리가 없습니다.")?;
        let mut paths: HashMap<u64, String> = HashMap::new();
        paths.insert(root_ref, root);
        fn resolve(
            frn: u64,
            snapshot: &Snapshot,
            dirs: &HashMap<u64, usize>,
            paths: &mut HashMap<u64, String>,
            depth: usize,
        ) -> Option<()> {
            if paths.contains_key(&frn) {
                return Some(());
            }
            if depth > 256 {
                return None;
            }
            let node = &snapshot.nodes[*dirs.get(&frn)?];
            if node.parent == frn {
                return None;
            }
            // Do not index entries below junction/symlink directories.
            if node.flags & NODE_REPARSE != 0 {
                return None;
            }
            resolve(node.parent, snapshot, dirs, paths, depth + 1)?;
            let parent = paths.get(&node.parent)?;
            let path = format!(
                "{}{}{}",
                parent,
                if parent.ends_with('\\') { "" } else { "\\" },
                snapshot.name(node)
            );
            paths.insert(frn, path);
            Some(())
        }
        let mut folder_ids: HashMap<u64, u32> = HashMap::new();
        for node in &snapshot.nodes {
            if node.frn & FRN_MASK < 16 {
                continue;
            }
            if resolve(node.parent, snapshot, &directory_nodes, &mut paths, 0).is_none() {
                index.skipped += 1;
                continue;
            }
            let parent = &paths[&node.parent];
            let name = snapshot.name(node);
            let is_dir = node.flags & NODE_DIRECTORY != 0;
            // A whole-volume search needs no per-file path allocation or
            // lowercase scope check. Folder paths still populate the tree.
            let full = (!whole_volume || is_dir).then(|| {
                format!(
                    "{parent}{}{name}",
                    if parent.ends_with('\\') { "" } else { "\\" }
                )
            });
            if !whole_volume {
                let key = normalized(full.as_ref().unwrap());
                if !roots.iter().any(|r| child(&key, r)) || roots.contains(&key) {
                    continue;
                }
            }
            let id = if let Some(&id) = folder_ids.get(&node.parent) {
                id
            } else {
                let id = u32::try_from(index.folders.len())
                    .map_err(|_| "NTFS 폴더 수가 너무 많습니다.")?;
                index.folders.push(parent.clone());
                folder_ids.insert(node.parent, id);
                id
            };
            index.entries.push_view(EntryView {
                name,
                parent: id,
                size: (!is_dir && node.size != MISSING_SIZE).then_some(node.size),
                created: (node.created != MISSING_TIME).then_some(node.created),
                modified: (node.modified != MISSING_TIME).then_some(node.modified),
                is_dir,
            });
            // Empty directories need public folder IDs too, so a subsequent
            // ordinary watcher creation can attach children without a rebuild.
            if is_dir && node.flags & NODE_REPARSE == 0 && !folder_ids.contains_key(&node.frn) {
                let id = u32::try_from(index.folders.len())
                    .map_err(|_| "NTFS 폴더 수가 너무 많습니다.")?;
                index.folders.push(full.unwrap());
                folder_ids.insert(node.frn, id);
            }
        }
        index.skipped += snapshot.skipped;
        Ok(())
    }

    pub fn run(
        roots: &[PathBuf],
        excluded_folders: &[PathBuf],
        output: &Path,
        cancel: &AtomicBool,
        progress: impl Fn(ScanProgress),
        rebuild: bool,
    ) -> Result<FastReport, String> {
        let started = Instant::now();
        let exclusions = crate::exclusions::FolderExclusions::new(excluded_folders);
        let grouped = roots_by_drive(roots)?;
        let sidecar = output.with_extension("ntfs.bin");
        let mut timings = FastTimings::default();
        let phase = Instant::now();
        let mut snapshots = if rebuild {
            Vec::new()
        } else {
            super::load_snapshots(&sidecar).unwrap_or_default()
        };
        timings.load_ms = phase.elapsed().as_secs_f64() * 1000.0;
        let mut index = Index {
            roots: roots
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            built_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
            ..Index::default()
        };
        let mut replayed = 0;
        let mut rebuilt = 0;
        let mut changed = 0;
        let mut hydrated = 0;
        let mut drives: Vec<_> = grouped.keys().copied().collect();
        drives.sort_unstable();
        // Retain only requested volumes. Snapshot data is transient in helper.
        snapshots.retain(|s| grouped.contains_key(&s.drive));
        for drive in drives {
            let phase = Instant::now();
            let volume = Volume::open(drive)?;
            let position = snapshots.iter().position(|s| {
                s.drive == drive && s.serial == volume.data.VolumeSerialNumber as u64
            });
            let ready = position.and_then(|i| match volume.replay(&mut snapshots[i], cancel) {
                Ok(count) => {
                    changed += count;
                    replayed += 1;
                    Some(i)
                }
                Err(_) => None,
            });
            let position = if let Some(i) = ready {
                i
            } else {
                snapshots.retain(|s| s.drive != drive);
                snapshots.push(volume.scan(cancel, &progress, started)?);
                rebuilt += 1;
                snapshots.len() - 1
            };
            timings.replay_ms += phase.elapsed().as_secs_f64() * 1000.0;
            let phase = Instant::now();
            hydrated +=
                volume.hydrate_incomplete(&mut snapshots[position], cancel, &progress, started)?;
            timings.hydrate_ms += phase.elapsed().as_secs_f64() * 1000.0;
            let phase = Instant::now();
            append_index(&mut index, &snapshots[position], &grouped[&drive])?;
            timings.build_ms += phase.elapsed().as_secs_f64() * 1000.0;
        }
        exclusions.apply(&mut index);
        index.scan_ms = started.elapsed().as_millis() as u64;
        index.compact();
        let phase = Instant::now();
        super::save_snapshots(&sidecar, &snapshots)?;
        timings.sidecar_save_ms = phase.elapsed().as_secs_f64() * 1000.0;
        let phase = Instant::now();
        crate::cache::save(output, &index)?;
        timings.cache_save_ms = phase.elapsed().as_secs_f64() * 1000.0;
        progress(ScanProgress {
            entries: index.entries.len(),
            folders: index.folders.len(),
            skipped: index.skipped,
            elapsed_ms: started.elapsed().as_millis() as u64,
        });
        Ok(FastReport {
            entries: index.entries.len(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            replayed_volumes: replayed,
            rebuilt_volumes: rebuilt,
            changed_records: changed,
            hydrated_records: hydrated,
            source: "NTFS MFT + USN",
            timings,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn directory_links_hardlinks_and_scope_reconstruct_without_following_junctions() {
            let seq = 1_u64 << 48;
            let mut s = Snapshot {
                drive: b'C',
                ..Snapshot::default()
            };
            for (number, parent, name, is_dir, is_reparse) in [
                (5, 5, ".", true, false),
                (100, 5, "자료", true, false),
                (200, 100, "보고서.txt", false, false),
                (200, 5, "다른 이름.txt", false, false),
                (300, 5, "junction", true, true),
                (400, 300, "not-followed.txt", false, false),
            ] {
                s.add(&Parsed {
                    frn: number | seq,
                    is_dir,
                    is_reparse,
                    names: vec![FileName {
                        parent: parent | seq,
                        namespace: 1,
                        name: name.into(),
                    }],
                    created: Some(100),
                    modified: Some(200),
                    size: Some(42),
                    ..Parsed::default()
                })
                .unwrap();
            }
            let mut index = Index::default();
            append_index(&mut index, &s, &["c:".into()]).unwrap();
            assert_eq!(index.entries.len(), 4);
            assert!(!index.entries.iter().any(|e| e.name == "not-followed.txt"));
            let row = index
                .entries
                .iter()
                .position(|e| e.name == "보고서.txt")
                .unwrap();
            assert_eq!(index.full_path(row), "C:\\자료\\보고서.txt");
            assert_eq!(index.entry(row).modified, Some(200));
            assert!(index.folders.iter().any(|p| p == "C:\\자료"));
            let mut generic = Index::default();
            append_index(&mut generic, &s, &["c:".into(), "c:\\unused".into()]).unwrap();
            let materialize = |index: &Index| {
                (0..index.len())
                    .map(|row| {
                        let e = index.entry(row);
                        (
                            index.full_path(row),
                            e.size,
                            e.created,
                            e.modified,
                            e.is_dir,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(materialize(&index), materialize(&generic));
            let mut overlapping = Index::default();
            append_index(&mut overlapping, &s, &["c:".into(), "c:\\자료".into()]).unwrap();
            assert_eq!(overlapping.len(), 3);
            assert!(!overlapping.entries.iter().any(|e| e.name == "자료"));
            let mut scoped = Index::default();
            append_index(&mut scoped, &s, &["c:\\자료".into()]).unwrap();
            assert_eq!(scoped.entries.len(), 1);
            assert_eq!(scoped.entry(0).name, "보고서.txt");
        }
    }
}

#[cfg(windows)]
pub use win::run;
#[cfg(not(windows))]
pub fn run(
    _: &[PathBuf],
    _: &[PathBuf],
    _: &Path,
    _: &AtomicBool,
    _: impl Fn(ScanProgress),
    _: bool,
) -> Result<FastReport, String> {
    Err("Windows 전용 기능입니다.".into())
}

// Bounded binary sidecar. GUI processes never read it; only the short-lived
// helper loads records needed to reconstruct directory moves and hard links.
fn load_snapshots(path: &Path) -> Result<Vec<Snapshot>, String> {
    use std::io::{BufReader, Read};
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut remaining = file.metadata().map_err(|e| e.to_string())?.len();
    if remaining > 1024 * 1024 * 1024 {
        return Err("NTFS 상태 파일이 너무 큽니다.".into());
    }
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    fn read<const N: usize>(r: &mut impl Read, left: &mut u64) -> Result<[u8; N], String> {
        if *left < N as u64 {
            return Err("잘린 NTFS 상태 파일".into());
        }
        let mut b = [0; N];
        r.read_exact(&mut b).map_err(|e| e.to_string())?;
        *left -= N as u64;
        Ok(b)
    }
    if read::<8>(&mut reader, &mut remaining)? != *b"QFMFT003" {
        return Err("지원하지 않는 NTFS 상태 파일".into());
    }
    let count = u32::from_le_bytes(read(&mut reader, &mut remaining)?) as usize;
    if count > 26 {
        return Err("NTFS 볼륨 수가 올바르지 않습니다.".into());
    }
    let mut snapshots = Vec::new();
    for _ in 0..count {
        let drive = read::<1>(&mut reader, &mut remaining)?[0];
        if !drive.is_ascii_uppercase() {
            return Err("NTFS 드라이브 이름 오류".into());
        }
        let serial = u64::from_le_bytes(read(&mut reader, &mut remaining)?);
        let journal_id = u64::from_le_bytes(read(&mut reader, &mut remaining)?);
        let next_usn = i64::from_le_bytes(read(&mut reader, &mut remaining)?);
        let skipped = u64::from_le_bytes(read(&mut reader, &mut remaining)?);
        let n = u32::from_le_bytes(read(&mut reader, &mut remaining)?) as usize;
        let bytes = u32::from_le_bytes(read(&mut reader, &mut remaining)?) as usize;
        if n > 10_000_000 || bytes > 512 * 1024 * 1024 || n as u64 * 48 + bytes as u64 > remaining {
            return Err("NTFS 상태 파일 크기 오류".into());
        }
        let mut nodes = Vec::with_capacity(n);
        for _ in 0..n {
            let b = read::<48>(&mut reader, &mut remaining)?;
            nodes.push(Node {
                frn: u64_at(&b, 0).unwrap(),
                parent: u64_at(&b, 8).unwrap(),
                name: u32_at(&b, 16).unwrap(),
                name_len: u16_at(&b, 20).unwrap(),
                flags: u16_at(&b, 22).unwrap(),
                size: u64_at(&b, 24).unwrap(),
                created: i64_at(&b, 32).unwrap(),
                modified: i64_at(&b, 40).unwrap(),
            });
        }
        let mut names = vec![0; bytes];
        reader.read_exact(&mut names).map_err(|e| e.to_string())?;
        remaining -= bytes as u64;
        for node in &nodes {
            let start = node.name as usize;
            let end = start
                .checked_add(node.name_len as usize)
                .ok_or("NTFS 이름 범위 오류")?;
            let s = names
                .get(start..end)
                .and_then(|b| std::str::from_utf8(b).ok())
                .ok_or("NTFS 이름 범위 오류")?;
            if s.contains(['\\', '/', '\0']) || s.is_empty() {
                return Err("NTFS 이름 오류".into());
            }
        }
        snapshots.push(Snapshot {
            drive,
            serial,
            journal_id,
            next_usn,
            nodes,
            names,
            skipped,
        });
    }
    if remaining != 0 {
        return Err("NTFS 상태 파일에 추가 데이터가 있습니다.".into());
    }
    Ok(snapshots)
}
fn save_snapshots(path: &Path, snapshots: &[Snapshot]) -> Result<(), String> {
    use std::io::{BufWriter, Write};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temporary = path.with_extension(format!("ntfs.tmp.{}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        let mut w = BufWriter::with_capacity(1024 * 1024, file);
        w.write_all(b"QFMFT003")?;
        w.write_all(&(snapshots.len() as u32).to_le_bytes())?;
        for s in snapshots {
            w.write_all(&[s.drive])?;
            w.write_all(&s.serial.to_le_bytes())?;
            w.write_all(&s.journal_id.to_le_bytes())?;
            w.write_all(&s.next_usn.to_le_bytes())?;
            w.write_all(&s.skipped.to_le_bytes())?;
            w.write_all(&(s.nodes.len() as u32).to_le_bytes())?;
            w.write_all(&(s.names.len() as u32).to_le_bytes())?;
            for n in &s.nodes {
                let mut record = [0u8; 48];
                record[0..8].copy_from_slice(&n.frn.to_le_bytes());
                record[8..16].copy_from_slice(&n.parent.to_le_bytes());
                record[16..20].copy_from_slice(&n.name.to_le_bytes());
                record[20..22].copy_from_slice(&n.name_len.to_le_bytes());
                record[22..24].copy_from_slice(&n.flags.to_le_bytes());
                record[24..32].copy_from_slice(&n.size.to_le_bytes());
                record[32..40].copy_from_slice(&n.created.to_le_bytes());
                record[40..48].copy_from_slice(&n.modified.to_le_bytes());
                w.write_all(&record)?;
            }
            w.write_all(&s.names)?;
        }
        w.flush()?;
        w.get_ref().sync_all()?;
        drop(w);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|e| format!("NTFS 상태 저장 실패: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "opt-in read-only snapshot benchmark; writes only a unique private fixture"]
    fn snapshot_io_benchmark() {
        let source =
            PathBuf::from(std::env::var_os("QF_SNAPSHOT_BENCH_SOURCE").expect("source snapshot"));
        let folder = PathBuf::from(
            std::env::var_os("QF_SNAPSHOT_BENCH_DIR").expect("private output directory"),
        );
        std::fs::create_dir_all(&folder).unwrap();
        let target = folder.join(format!("snapshot-bench-{}.ntfs", std::process::id()));
        assert!(!target.exists());
        let start = Instant::now();
        let snapshots = load_snapshots(&source).unwrap();
        let load_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        save_snapshots(&target, &snapshots).unwrap();
        let save_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(
            std::fs::metadata(&target).unwrap().len(),
            std::fs::metadata(&source).unwrap().len()
        );
        let mut index = Index::default();
        let start = Instant::now();
        for snapshot in &snapshots {
            win::append_index(
                &mut index,
                snapshot,
                &[format!("{}:", snapshot.drive as char).to_lowercase()],
            )
            .unwrap();
        }
        index.compact();
        let build_ms = start.elapsed().as_secs_f64() * 1000.0;
        println!(
            "{}",
            serde_json::json!({"snapshot_load_ms":load_ms,"snapshot_save_ms":save_ms,
                "nodes":snapshots.iter().map(|s| s.nodes.len()).sum::<usize>(),
                "build_ms":build_ms,"entries":index.len(),"index_allocated_bytes":index.allocated_bytes()})
        );
        std::fs::remove_file(&target).unwrap();
    }
    fn record() -> Vec<u8> {
        let mut b = vec![0; 1024];
        b[..4].copy_from_slice(b"FILE");
        b[4..6].copy_from_slice(&48_u16.to_le_bytes());
        b[6..8].copy_from_slice(&3_u16.to_le_bytes());
        b[16..18].copy_from_slice(&7_u16.to_le_bytes());
        b[20..22].copy_from_slice(&56_u16.to_le_bytes());
        b[22..24].copy_from_slice(&1_u16.to_le_bytes());
        b[24..28].copy_from_slice(&64_u32.to_le_bytes());
        b[28..32].copy_from_slice(&1024_u32.to_le_bytes());
        b[48..50].copy_from_slice(&0xabcd_u16.to_le_bytes());
        b[50..52].copy_from_slice(&0x1234_u16.to_le_bytes());
        b[52..54].copy_from_slice(&0x5678_u16.to_le_bytes());
        b[56..60].copy_from_slice(&u32::MAX.to_le_bytes());
        b[510..512].copy_from_slice(&0xabcd_u16.to_le_bytes());
        b[1022..1024].copy_from_slice(&0xabcd_u16.to_le_bytes());
        b
    }
    #[test]
    fn fixups_restore_trailers_and_reject_torn_records() {
        let mut b = record();
        fixup(&mut b).unwrap();
        assert_eq!(u16_at(&b, 510), Some(0x1234));
        assert_eq!(u16_at(&b, 1022), Some(0x5678));
        let mut torn = record();
        torn[1022] = 0;
        assert!(fixup(&mut torn).is_err());
    }
    #[test]
    fn ioctl_restored_first_word_equal_to_tag_is_not_a_torn_record() {
        let mut restored = record();
        restored[50..52].copy_from_slice(&0xabcd_u16.to_le_bytes());
        fixup(&mut restored).unwrap();
        assert_eq!(u16_at(&restored, 510), Some(0xabcd));
        assert_eq!(u16_at(&restored, 1022), Some(0x5678));
        normalize_ioctl_record(&mut restored).unwrap();
        let mut raw = record();
        normalize_ioctl_record(&mut raw).unwrap();
        assert_eq!(u16_at(&raw, 510), Some(0x1234));
        // Cached logical data can also differ from both the tag and the stale
        // saved word. Its header/attributes remain valid and must be accepted.
        restored[510..512].copy_from_slice(&0x9999_u16.to_le_bytes());
        restored[1022..1024].copy_from_slice(&0x8888_u16.to_le_bytes());
        normalize_ioctl_record(&mut restored).unwrap();
        let mut invalid = record();
        invalid[1022..1024].copy_from_slice(&0x9876_u16.to_le_bytes());
        invalid[24..28].copy_from_slice(&2048_u32.to_le_bytes());
        assert!(normalize_ioctl_record(&mut invalid).is_err());
    }
    #[test]
    fn runlist_sign_extension_and_fragment_offsets() {
        let mut a = vec![0; 80];
        a[8] = 1;
        a[32..34].copy_from_slice(&64_u16.to_le_bytes());
        a[64..71].copy_from_slice(&[0x11, 8, 100, 0x11, 4, 0xfc, 0]);
        let runs = parse_runs(&a).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].vcn, runs[0].lcn, runs[0].clusters), (0, 100, 8));
        assert_eq!((runs[1].vcn, runs[1].lcn, runs[1].clusters), (8, 96, 4));
    }
    #[test]
    fn rejects_bad_attribute_lengths_without_panicking() {
        let mut b = record();
        fixup(&mut b).unwrap();
        b[56..60].copy_from_slice(&0x30_u32.to_le_bytes());
        b[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_record(&b, 42).is_err());
    }
    #[test]
    fn parser_reads_korean_name_and_real_file_metadata() {
        fn resident_attr(kind: u32, value: &[u8]) -> Vec<u8> {
            let size = (24 + value.len() + 7) & !7;
            let mut b = vec![0; size];
            b[..4].copy_from_slice(&kind.to_le_bytes());
            b[4..8].copy_from_slice(&(size as u32).to_le_bytes());
            b[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
            b[20..22].copy_from_slice(&24_u16.to_le_bytes());
            b[24..24 + value.len()].copy_from_slice(value);
            b
        }
        let mut b = record();
        let ticks = (1_700_000_000_u64 + 11_644_473_600) * 10_000_000;
        let mut standard = vec![0; 72];
        standard[..8].copy_from_slice(&ticks.to_le_bytes());
        standard[8..16].copy_from_slice(&(ticks + 30 * 10_000_000).to_le_bytes());
        let name: Vec<u16> = "보고서_2026.pdf".encode_utf16().collect();
        let mut filename = vec![0; 66 + name.len() * 2];
        filename[..8].copy_from_slice(&(5_u64 | (1 << 48)).to_le_bytes());
        filename[64] = name.len() as u8;
        filename[65] = 1;
        for (i, c) in name.iter().enumerate() {
            filename[66 + i * 2..68 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        let mut offset = 56;
        for a in [
            resident_attr(0x10, &standard),
            resident_attr(0x30, &filename),
            resident_attr(0x80, b"hello"),
        ] {
            b[offset..offset + a.len()].copy_from_slice(&a);
            offset += a.len();
        }
        b[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        b[24..28].copy_from_slice(&((offset + 8) as u32).to_le_bytes());
        fixup(&mut b).unwrap();
        let parsed = parse_record(&b, 42).unwrap().unwrap();
        assert_eq!(parsed.frn, 42 | (7 << 48));
        assert_eq!(parsed.names[0].name, "보고서_2026.pdf");
        assert_eq!(parsed.size, Some(5));
        assert_eq!(parsed.created, Some(1_700_000_000));
        assert_eq!(parsed.modified, Some(1_700_000_030));
    }
    #[test]
    fn extension_only_names_and_later_data_segments_are_not_lost() {
        use std::collections::HashMap;
        let seq = 1_u64 << 48;
        let mut s = Snapshot::default();
        s.add(&Parsed {
            frn: 100 | seq,
            names: vec![FileName {
                parent: 5 | seq,
                namespace: 1,
                name: "base.txt".into(),
            }],
            ..Parsed::default()
        })
        .unwrap();
        let mut bases = HashMap::new();
        bases.insert(
            200 | seq,
            Parsed {
                frn: 200 | seq,
                is_dir: true,
                created: Some(1000),
                modified: Some(2000),
                ..Parsed::default()
            },
        );
        bases.insert(
            300 | seq,
            Parsed {
                frn: 300 | seq,
                created: Some(3000),
                modified: Some(4000),
                ..Parsed::default()
            },
        );
        bases.insert(
            400 | seq,
            Parsed {
                frn: 400 | seq,
                size: Some(7),
                names: vec![FileName {
                    parent: 5 | seq,
                    namespace: 2,
                    name: "DOSONLY.TXT".into(),
                }],
                ..Parsed::default()
            },
        );
        merge_extensions(
            &mut s,
            bases,
            vec![
                Parsed {
                    frn: 501 | seq,
                    base: 100 | seq,
                    names: vec![FileName {
                        parent: 5 | seq,
                        namespace: 1,
                        name: "alias.txt".into(),
                    }],
                    ..Parsed::default()
                },
                Parsed {
                    frn: 502 | seq,
                    base: 200 | seq,
                    names: vec![FileName {
                        parent: 5 | seq,
                        namespace: 1,
                        name: "확장 폴더".into(),
                    }],
                    ..Parsed::default()
                },
                Parsed {
                    frn: 503 | seq,
                    base: 300 | seq,
                    names: vec![FileName {
                        parent: 200 | seq,
                        namespace: 1,
                        name: "확장 파일.txt".into(),
                    }],
                    ..Parsed::default()
                },
                Parsed {
                    frn: 504 | seq,
                    base: 100 | seq,
                    size: Some(99),
                    ..Parsed::default()
                },
                Parsed {
                    frn: 505 | seq,
                    base: 300 | seq,
                    size: Some(123),
                    ..Parsed::default()
                },
            ],
        )
        .unwrap();
        assert_eq!(s.nodes.len(), 5);
        let dir = s.nodes.iter().find(|n| n.frn == 200 | seq).unwrap();
        assert_ne!(dir.flags & NODE_DIRECTORY, 0);
        assert_eq!(s.name(dir), "확장 폴더");
        assert_eq!(dir.modified, 2000);
        assert!(
            s.nodes
                .iter()
                .filter(|n| n.frn == 100 | seq)
                .all(|n| n.size == 99)
        );
        let file = s.nodes.iter().find(|n| n.frn == 300 | seq).unwrap();
        assert_eq!(s.name(file), "확장 파일.txt");
        assert_eq!(file.size, 123);
        assert_eq!(file.created, 3000);
        assert!(s.nodes.iter().any(|n| s.name(n) == "DOSONLY.TXT"));
    }
    #[test]
    fn replay_refreshes_old_new_and_deleted_child_parents() {
        use std::collections::HashSet;
        let seq = 1_u64 << 48;
        let mut snapshot = Snapshot::default();
        snapshot
            .add(&Parsed {
                frn: 100 | seq,
                names: vec![FileName {
                    parent: 10 | seq,
                    namespace: 1,
                    name: "renamed.txt".into(),
                }],
                ..Parsed::default()
            })
            .unwrap();
        snapshot
            .add(&Parsed {
                frn: 200 | seq,
                names: vec![FileName {
                    parent: 30 | seq,
                    namespace: 1,
                    name: "deleted.txt".into(),
                }],
                ..Parsed::default()
            })
            .unwrap();
        let refreshed = vec![Parsed {
            frn: 100 | seq,
            names: vec![FileName {
                parent: 20 | seq,
                namespace: 1,
                name: "moved.txt".into(),
            }],
            ..Parsed::default()
        }];
        let parents = affected_parent_references(&snapshot, &HashSet::from([100, 200]), &refreshed);
        assert_eq!(parents, HashSet::from([10 | seq, 20 | seq, 30 | seq]));
    }
    #[test]
    fn sidecar_roundtrip_and_bounded_corruption() {
        let p = std::env::temp_dir().join(format!(
            "qf-mft-sidecar-{}-{}.bin",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut s = Snapshot {
            drive: b'C',
            serial: 123,
            journal_id: 42,
            next_usn: 100,
            ..Snapshot::default()
        };
        s.add(&Parsed {
            frn: 123,
            names: vec![FileName {
                parent: 5,
                namespace: 1,
                name: "보고서.txt".into(),
            }],
            size: Some(42),
            ..Parsed::default()
        })
        .unwrap();
        save_snapshots(&p, &[s]).unwrap();
        let loaded = load_snapshots(&p).unwrap();
        assert_eq!(loaded[0].name(&loaded[0].nodes[0]), "보고서.txt");
        assert_eq!(loaded[0].nodes[0].size, 42);
        let mut bytes = std::fs::read(&p).unwrap();
        bytes.truncate(bytes.len() - 1);
        std::fs::write(&p, bytes).unwrap();
        assert!(load_snapshots(&p).is_err());
        std::fs::remove_file(p).unwrap();
    }
}
