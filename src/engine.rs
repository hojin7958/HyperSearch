use crate::model::Index;
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    sync::{Arc, OnceLock, Weak},
    time::Instant,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchMode {
    Plain,
    Wildcard,
    Regex,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    All,
    Files,
    Folders,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortField {
    Name,
    Path,
    Size,
    Created,
    Modified,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchOptions {
    pub mode: SearchMode,
    pub match_path: bool,
    pub case_sensitive: bool,
    pub kind: EntryKind,
    pub sort: SortField,
    pub descending: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            mode: SearchMode::Plain,
            match_path: false,
            case_sensitive: false,
            kind: EntryKind::All,
            sort: SortField::Modified,
            descending: true,
        }
    }
}

pub struct SearchResult {
    pub rows: Vec<u32>,
    pub elapsed_ms: f64,
    pub scanned_rows: usize,
}
const MAX_CANDIDATES: usize = 65_536;
struct PreviousSearch {
    engine: Weak<SearchEngine>,
    query: String,
    options: SearchOptions,
    rows: Vec<u32>,
}
/// One bounded candidate list, without keeping an old index alive during updates.
#[derive(Default)]
pub struct SearchSession {
    previous: Option<PreviousSearch>,
}
impl SearchSession {
    pub fn search(
        &mut self,
        engine: &Arc<SearchEngine>,
        query: &str,
        options: &SearchOptions,
    ) -> Result<SearchResult, String> {
        let previous = self.previous.as_ref().filter(|previous| {
            Weak::ptr_eq(&previous.engine, &Arc::downgrade(engine))
                && same_predicate_options(&previous.options, options)
                && narrows_query(&previous.query, query, options.mode)
        });
        let ordered = previous.is_some_and(|p| {
            p.options.sort == options.sort && p.options.descending == options.descending
        });
        let result =
            engine.search_candidates(query, options, previous.map(|p| p.rows.as_slice()), ordered);
        self.previous = result
            .as_ref()
            .ok()
            .filter(|r| r.rows.len() <= MAX_CANDIDATES)
            .map(|r| PreviousSearch {
                engine: Arc::downgrade(engine),
                query: query.into(),
                options: options.clone(),
                rows: r.rows.clone(),
            });
        result
    }
}
fn same_predicate_options(a: &SearchOptions, b: &SearchOptions) -> bool {
    a.mode == b.mode
        && a.match_path == b.match_path
        && a.case_sensitive == b.case_sensitive
        && a.kind == b.kind
}
fn narrows_query(previous: &str, next: &str, mode: SearchMode) -> bool {
    if previous == next || previous.is_empty() {
        return true;
    }
    match mode {
        SearchMode::Plain => next.starts_with(previous),
        SearchMode::Wildcard => {
            let contains_literal = |query: &str| {
                if query.starts_with('*') && query.ends_with('*') {
                    let middle = query.trim_matches('*');
                    (!middle.contains(['*', '?'])).then_some(middle.to_owned())
                } else {
                    None
                }
            };
            match (contains_literal(previous), contains_literal(next)) {
                (Some(a), Some(b)) => b.starts_with(&a),
                _ => false,
            }
        }
        SearchMode::Regex => false,
    }
}
struct SortedRows {
    field: SortField,
    ids: Vec<u32>,
    known: usize,
}
/// No normalized-name copies; only one compact sorted row array is retained.
pub struct SearchEngine {
    index: Arc<Index>,
    sorted: std::sync::Mutex<Option<SortedRows>>,
    folder_ranks: OnceLock<Vec<u32>>,
}
impl SearchEngine {
    pub fn new(mut index: Arc<Index>) -> Self {
        if let Some(unique) = Arc::get_mut(&mut index) {
            unique.compact();
        }
        Self {
            index,
            sorted: std::sync::Mutex::new(None),
            folder_ranks: OnceLock::new(),
        }
    }
    pub fn index(&self) -> &Arc<Index> {
        &self.index
    }
    pub fn into_index(self) -> Arc<Index> {
        self.index
    }
    pub fn search(&self, query: &str, options: &SearchOptions) -> Result<SearchResult, String> {
        self.search_candidates(query, options, None, false)
    }
    fn search_candidates(
        &self,
        query: &str,
        options: &SearchOptions,
        candidates: Option<&[u32]>,
        ordered: bool,
    ) -> Result<SearchResult, String> {
        let started = Instant::now();
        let matcher = Matcher::new(query, options)?;
        if options.sort == SortField::Path {
            self.folder_ranks();
        }
        let matches = |row: u32, scratch: &mut MatchScratch| {
            let e = self.index.entry(row as usize);
            if (options.kind == EntryKind::Files && e.is_dir)
                || (options.kind == EntryKind::Folders && !e.is_dir)
            {
                return false;
            }
            if matches!(scratch.matcher, Matcher::All) {
                return true;
            }
            if options.match_path {
                if scratch.parent != Some(e.parent) {
                    self.index.folders.write_path(
                        e.parent as usize,
                        &mut scratch.path,
                        &mut scratch.ancestry,
                    );
                    if !scratch.path.ends_with(['\\', '/']) && !scratch.path.is_empty() {
                        scratch.path.push('\\');
                    }
                    scratch.parent = Some(e.parent);
                    scratch.parent_len = scratch.path.len();
                } else {
                    scratch.path.truncate(scratch.parent_len);
                }
                scratch.path.push_str(e.name);
                scratch.matcher.matches(Text::new("", &scratch.path, false))
            } else {
                scratch.matcher.matches(Text::new("", e.name, false))
            }
        };
        let scanned_rows = candidates.map_or(self.index.len(), <[u32]>::len);
        let mut rows: Vec<u32> = if let Some(candidates) = candidates {
            let mut scratch = MatchScratch::new(&matcher);
            candidates
                .iter()
                .copied()
                .filter(|&row| matches(row, &mut scratch))
                .collect()
        } else if query.is_empty() && options.kind == EntryKind::All {
            (0..self.index.len() as u32).collect()
        } else if self.index.len() >= 100_000 {
            (0..self.index.len() as u32)
                .into_par_iter()
                .with_min_len(8192)
                .map_init(
                    || MatchScratch::new(&matcher),
                    |scratch, row| matches(row, scratch).then_some(row),
                )
                .filter_map(|row| row)
                .collect()
        } else {
            let mut scratch = MatchScratch::new(&matcher);
            (0..self.index.len() as u32)
                .filter(|&row| matches(row, &mut scratch))
                .collect()
        };
        if ordered {
            return Ok(SearchResult {
                rows,
                elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                scanned_rows,
            });
        }
        let mut cache = self.sorted.lock().unwrap_or_else(|p| p.into_inner());
        let warm = cache.as_ref().is_some_and(|c| c.field == options.sort);
        let use_cache = !rows.is_empty()
            && (rows.len() >= 50_000
                || (rows.len() >= 4096 && warm)
                || rows.len() >= self.index.len().max(1) / 16);
        if !use_cache {
            rows.sort_unstable_by(|&a, &b| {
                self.compare(a as usize, b as usize, options.sort, options.descending)
            });
        } else {
            if !warm {
                *cache = None;
                let mut ids: Vec<u32> = (0..self.index.len() as u32).collect();
                if ids.len() >= 50_000 {
                    ids.par_sort_unstable_by(|&a, &b| {
                        self.compare(a as usize, b as usize, options.sort, false)
                    });
                } else {
                    ids.sort_unstable_by(|&a, &b| {
                        self.compare(a as usize, b as usize, options.sort, false)
                    });
                }
                let known = ids.partition_point(|&row| self.known(row as usize, options.sort));
                *cache = Some(SortedRows {
                    field: options.sort,
                    ids,
                    known,
                });
            }
            let sorted = cache.as_ref().unwrap();
            if rows.len() == self.index.len() {
                rows.copy_from_slice(&sorted.ids);
                if options.descending {
                    rows[..sorted.known].reverse();
                }
            } else {
                let mut included = vec![0u64; self.index.len().div_ceil(64)];
                for &row in &rows {
                    included[row as usize / 64] |= 1 << (row % 64);
                }
                rows.clear();
                let mut add = |&row: &u32| {
                    if included[row as usize / 64] & (1 << (row % 64)) != 0 {
                        rows.push(row);
                    }
                };
                if options.descending {
                    sorted.ids[..sorted.known].iter().rev().for_each(&mut add);
                } else {
                    sorted.ids[..sorted.known].iter().for_each(&mut add);
                }
                sorted.ids[sorted.known..].iter().for_each(&mut add);
            }
        }
        Ok(SearchResult {
            rows,
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            scanned_rows,
        })
    }
    fn folder_ranks(&self) -> &[u32] {
        self.folder_ranks.get_or_init(|| {
            let mut paths: Vec<(String, u32)> = self
                .index
                .folders
                .iter()
                .enumerate()
                .map(|(id, path)| (path.to_lowercase(), id as u32))
                .collect();
            paths.par_sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
            let mut ranks = vec![0; paths.len()];
            let mut rank = 0;
            for i in 0..paths.len() {
                if i > 0 && paths[i - 1].0 != paths[i].0 {
                    rank += 1;
                }
                ranks[paths[i].1 as usize] = rank;
            }
            ranks
        })
    }
    fn known(&self, row: usize, field: SortField) -> bool {
        let e = self.index.entry(row);
        match field {
            SortField::Size => !e.is_dir && e.size.is_some(),
            SortField::Created => e.created.is_some(),
            SortField::Modified => e.modified.is_some(),
            _ => true,
        }
    }
    fn compare(&self, a: usize, b: usize, field: SortField, descending: bool) -> Ordering {
        let known_a = self.known(a, field);
        let known_b = self.known(b, field);
        if known_a != known_b {
            return known_b.cmp(&known_a);
        }
        let ea = self.index.entry(a);
        let eb = self.index.entry(b);
        let order = (if !known_a {
            Ordering::Equal
        } else {
            match field {
                SortField::Name => case_cmp(ea.name, eb.name),
                SortField::Path => self.folder_ranks()[ea.parent as usize]
                    .cmp(&self.folder_ranks()[eb.parent as usize]),
                SortField::Size => ea.size.cmp(&eb.size),
                SortField::Created => ea.created.cmp(&eb.created),
                SortField::Modified => ea.modified.cmp(&eb.modified),
            }
        })
        .then_with(|| case_cmp(ea.name, eb.name))
        .then_with(|| ea.parent.cmp(&eb.parent))
        .then_with(|| a.cmp(&b));
        if descending && known_a {
            order.reverse()
        } else {
            order
        }
    }
}
fn case_cmp(a: &str, b: &str) -> Ordering {
    if a.is_ascii() && b.is_ascii() {
        a.bytes()
            .map(|b| b.to_ascii_lowercase())
            .cmp(b.bytes().map(|b| b.to_ascii_lowercase()))
    } else {
        a.chars()
            .flat_map(char::to_lowercase)
            .cmp(b.chars().flat_map(char::to_lowercase))
    }
}

/// A filename or full path represented without allocating a path per match.
#[derive(Clone, Copy)]
struct Text<'a> {
    parent: &'a str,
    separator: &'static str,
    name: &'a str,
}
impl<'a> Text<'a> {
    fn new(parent: &'a str, name: &'a str, path: bool) -> Self {
        if !path {
            Self {
                parent: "",
                separator: "",
                name,
            }
        } else {
            Self {
                parent,
                separator: if parent.ends_with(['\\', '/']) || parent.is_empty() {
                    ""
                } else {
                    "\\"
                },
                name,
            }
        }
    }
    fn len(self) -> usize {
        self.parent.len() + self.separator.len() + self.name.len()
    }
    fn byte(self, index: usize) -> u8 {
        if index < self.parent.len() {
            self.parent.as_bytes()[index]
        } else if index < self.parent.len() + self.separator.len() {
            b'\\'
        } else {
            self.name.as_bytes()[index - self.parent.len() - self.separator.len()]
        }
    }
    fn next_char(self, offset: usize) -> usize {
        let first = self.byte(offset);
        offset
            + if first < 0x80 {
                1
            } else if first < 0xE0 {
                2
            } else if first < 0xF0 {
                3
            } else {
                4
            }
    }
    fn contains(self, finder: &memchr::memmem::Finder<'_>) -> bool {
        let needle = finder.needle();
        if needle.is_empty() {
            return true;
        }
        if self.parent.is_empty() {
            return finder.find(self.name.as_bytes()).is_some();
        }
        if finder.find(self.parent.as_bytes()).is_some()
            || finder.find(self.name.as_bytes()).is_some()
        {
            return true;
        }
        // Only positions crossing the parent/name boundary remain to check.
        let boundary = self.parent.len();
        let start = boundary.saturating_sub(needle.len().saturating_sub(1));
        let end =
            (boundary + self.separator.len()).min(self.len().saturating_sub(needle.len()) + 1);
        (start..end).any(|offset| {
            needle.len() <= self.len().saturating_sub(offset)
                && needle
                    .iter()
                    .enumerate()
                    .all(|(i, &byte)| self.byte(offset + i) == byte)
        })
    }
    fn owned(self) -> String {
        let mut s = String::with_capacity(self.len());
        s.push_str(self.parent);
        s.push_str(self.separator);
        s.push_str(self.name);
        s
    }
}

#[derive(Clone)]
enum Matcher {
    All,
    Plain(memchr::memmem::Finder<'static>),
    Wildcard(Vec<u8>),
    Prefix(Vec<u8>),
    Suffix(Vec<u8>),
    Exact(Vec<u8>),
    Regex(Regex),
}
struct MatchScratch {
    matcher: Matcher,
    path: String,
    ancestry: Vec<u32>,
    parent: Option<u32>,
    parent_len: usize,
}
impl MatchScratch {
    fn new(matcher: &Matcher) -> Self {
        Self {
            matcher: matcher.clone(),
            path: String::new(),
            ancestry: Vec::new(),
            parent: None,
            parent_len: 0,
        }
    }
}
impl Matcher {
    fn new(query: &str, options: &SearchOptions) -> Result<Self, String> {
        if query.is_empty() {
            return Ok(Self::All);
        }
        if options.mode == SearchMode::Wildcard && query.bytes().all(|b| b == b'*') {
            return Ok(Self::All);
        }
        if options.mode == SearchMode::Regex {
            return RegexBuilder::new(query)
                .case_insensitive(!options.case_sensitive)
                .size_limit(8 * 1024 * 1024)
                .build()
                .map(Self::Regex)
                .map_err(|e| format!("정규식 오류: {e}"));
        }
        if !options.case_sensitive {
            let expression = if options.mode == SearchMode::Wildcard {
                let mut expression = String::from(r"\A");
                for c in query.chars() {
                    match c {
                        '*' => expression.push_str(".*"),
                        '?' => expression.push('.'),
                        c => expression.push_str(&regex::escape(&c.to_string())),
                    }
                }
                expression.push_str(r"\z");
                expression
            } else {
                regex::escape(query)
            };
            return RegexBuilder::new(&expression)
                .case_insensitive(true)
                .dot_matches_new_line(true)
                .size_limit(8 * 1024 * 1024)
                .build()
                .map(Self::Regex)
                .map_err(|e| format!("검색식 오류: {e}"));
        }
        let query = if options.case_sensitive {
            query.to_owned()
        } else {
            query.to_lowercase()
        };
        if options.mode == SearchMode::Wildcard {
            let middle = query.trim_matches('*');
            if !middle.contains(['*', '?']) {
                let leading = query.starts_with('*');
                let trailing = query.ends_with('*');
                return Ok(match (leading, trailing) {
                    (true, true) if middle.is_empty() => Self::All,
                    (true, true) => {
                        Self::Plain(memchr::memmem::Finder::new(middle.as_bytes()).into_owned())
                    }
                    (true, false) => Self::Suffix(middle.as_bytes().to_vec()),
                    (false, true) => Self::Prefix(middle.as_bytes().to_vec()),
                    (false, false) => Self::Exact(middle.as_bytes().to_vec()),
                });
            }
            return Ok(Self::Wildcard(query.into_bytes()));
        }
        Ok(Self::Plain(
            memchr::memmem::Finder::new(query.as_bytes()).into_owned(),
        ))
    }
    fn matches(&self, text: Text<'_>) -> bool {
        match self {
            Self::All => true,
            Self::Plain(finder) => text.contains(finder),
            Self::Wildcard(pattern) => wildcard(pattern, text),
            Self::Prefix(needle) => {
                needle.len() <= text.len()
                    && needle.iter().enumerate().all(|(i, &b)| text.byte(i) == b)
            }
            Self::Suffix(needle) => {
                needle.len() <= text.len()
                    && needle
                        .iter()
                        .enumerate()
                        .all(|(i, &b)| text.byte(text.len() - needle.len() + i) == b)
            }
            Self::Exact(needle) => {
                needle.len() == text.len()
                    && needle.iter().enumerate().all(|(i, &b)| text.byte(i) == b)
            }
            Self::Regex(regex) => {
                if text.parent.is_empty() {
                    regex.is_match(text.name)
                } else {
                    regex.is_match(&text.owned())
                }
            }
        }
    }
}

fn wildcard(pattern: &[u8], text: Text<'_>) -> bool {
    let (mut p, mut t) = (0, 0);
    let (mut star, mut retry) = (None, 0);
    while t < text.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = t;
        } else if p < pattern.len() && pattern[p] == b'?' {
            p += 1;
            t = text.next_char(t);
        } else if p < pattern.len() && pattern[p] == text.byte(t) {
            p += 1;
            t += 1;
        } else if let Some(previous_star) = star {
            // The retry cursor always advances by one Unicode scalar, making ?
            // one Korean character as well as one ASCII character.
            if retry >= text.len() {
                return false;
            }
            retry = text.next_char(retry);
            t = retry;
            p = previous_star + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Entry;

    fn engine() -> SearchEngine {
        let entries = [
            ("보고서_한글.PDF", 0, Some(20), Some(100), Some(300), false),
            ("보고서_12.pdf", 0, Some(10), Some(200), None, false),
            ("{{변수}}.txt", 1, None, None, Some(200), false),
            ("Documents", 0, Some(999), Some(50), Some(100), true),
        ]
        .into_iter()
        .map(|(name, parent, size, created, modified, is_dir)| Entry {
            name: name.into(),
            parent,
            size,
            created,
            modified,
            is_dir,
        })
        .collect();
        SearchEngine::new(Arc::new(Index {
            entries,
            folders: vec!["C:\\자료".into(), "D:\\Archive".into()].into(),
            ..Default::default()
        }))
    }

    #[test]
    fn incremental_queries_preserve_results_and_do_not_narrow_suffixes_or_regex_alternatives() {
        let engine = Arc::new(engine());
        let mut session = SearchSession::default();
        let mut options = SearchOptions::default();
        let first = session.search(&engine, "보", &options).unwrap();
        let next = session.search(&engine, "보고", &options).unwrap();
        assert_eq!(next.scanned_rows, first.rows.len());
        assert_eq!(next.rows, engine.search("보고", &options).unwrap().rows);
        assert_eq!(
            session
                .search(&engine, "보", &options)
                .unwrap()
                .scanned_rows,
            engine.index.len()
        );
        options.mode = SearchMode::Wildcard;
        session.search(&engine, "*.p", &options).unwrap();
        let suffix = session.search(&engine, "*.pdf", &options).unwrap();
        assert_eq!(suffix.rows.len(), 2);
        assert_eq!(suffix.scanned_rows, engine.index.len());
        session.search(&engine, "*보*", &options).unwrap();
        let contains = session.search(&engine, "*보고*", &options).unwrap();
        assert_eq!(contains.scanned_rows, 2);
        options.mode = SearchMode::Regex;
        session.search(&engine, "PDF", &options).unwrap();
        let alternatives = session.search(&engine, "PDF|txt", &options).unwrap();
        assert_eq!(
            alternatives.rows,
            engine.search("PDF|txt", &options).unwrap().rows
        );
        assert_eq!(alternatives.scanned_rows, engine.index.len());
        assert!(session.search(&engine, "[", &options).is_err());
        assert!(session.previous.is_none());
    }
    #[test]
    fn candidate_cache_respects_options_sort_order_and_index_identity_without_owning_an_index() {
        let engine = Arc::new(engine());
        let mut session = SearchSession::default();
        let mut options = SearchOptions::default();
        session.search(&engine, "보고", &options).unwrap();
        for field in [
            SortField::Name,
            SortField::Path,
            SortField::Size,
            SortField::Created,
            SortField::Modified,
        ] {
            options.sort = field;
            for descending in [true, false] {
                options.descending = descending;
                assert_eq!(
                    session.search(&engine, "보고", &options).unwrap().rows,
                    engine.search("보고", &options).unwrap().rows
                );
            }
        }
        options.case_sensitive = true;
        assert_eq!(
            session
                .search(&engine, "보고", &options)
                .unwrap()
                .scanned_rows,
            engine.index.len()
        );
        assert_eq!(Arc::strong_count(&engine), 1);
        let weak = Arc::downgrade(&engine);
        drop(engine);
        assert!(weak.upgrade().is_none());
        let replacement = Arc::new(super::tests::engine());
        assert_eq!(
            session
                .search(&replacement, "보고서", &options)
                .unwrap()
                .scanned_rows,
            replacement.index.len()
        );
    }
    #[test]
    fn broad_candidates_are_not_retained_and_zero_matches_can_safely_narrow() {
        let entries = (0..MAX_CANDIDATES + 1)
            .map(|i| Entry {
                name: format!("file_{i}.txt"),
                parent: 0,
                size: None,
                created: None,
                modified: None,
                is_dir: false,
            })
            .collect();
        let engine = Arc::new(SearchEngine::new(Arc::new(Index::from_parts(
            entries,
            vec![r"C:\data".into()],
        ))));
        let mut session = SearchSession::default();
        let options = SearchOptions::default();
        session.search(&engine, "", &options).unwrap();
        assert!(session.previous.is_none());
        assert!(
            session
                .search(&engine, "notfound", &options)
                .unwrap()
                .rows
                .is_empty()
        );
        assert_eq!(
            session
                .search(&engine, "notfound_more", &options)
                .unwrap()
                .scanned_rows,
            0
        );
    }
    #[test]
    fn parallel_full_path_scratch_matches_reference_unicode_regex_and_wildcard_semantics() {
        let folders = vec![
            r"C:\자료".into(),
            r"D:\한글\새 폴더".into(),
            r"\\server\share\folder".into(),
            r"C:\".into(),
        ];
        let entries = (0..100_010)
            .map(|i| Entry {
                name: format!(
                    "{}_{}.{}",
                    ["보고서", "File", "Kelvin", "ſample"][i % 4],
                    i,
                    if i % 2 == 0 { "PDF" } else { "txt" }
                ),
                parent: (i % 4) as u32,
                size: None,
                created: None,
                modified: Some(i as i64),
                is_dir: false,
            })
            .collect();
        let engine = SearchEngine::new(Arc::new(Index::from_parts(entries, folders)));
        for (query, mode) in [
            (r"자료\보고서", SearchMode::Plain),
            (r"*한글\*?.txt", SearchMode::Wildcard),
            (r"\.(pdf|txt)$", SearchMode::Regex),
            ("kelvin", SearchMode::Plain),
        ] {
            for case_sensitive in [true, false] {
                let options = SearchOptions {
                    mode,
                    match_path: true,
                    case_sensitive,
                    ..Default::default()
                };
                let matcher = Matcher::new(query, &options).unwrap();
                let mut expected: Vec<u32> = (0..engine.index.len() as u32)
                    .filter(|&row| {
                        let e = engine.index.entry(row as usize);
                        matcher.matches(Text::new(
                            &engine.index.parent_path(row as usize),
                            e.name,
                            true,
                        ))
                    })
                    .collect();
                expected.sort_unstable_by(|&a, &b| {
                    engine.compare(a as usize, b as usize, options.sort, options.descending)
                });
                assert_eq!(
                    engine.search(query, &options).unwrap().rows,
                    expected,
                    "{query} / {case_sensitive}"
                );
            }
        }
    }

    #[test]
    fn wildcard_supports_unicode_and_literal_braces() {
        let engine = engine();
        let options = SearchOptions {
            mode: SearchMode::Wildcard,
            ..Default::default()
        };
        assert_eq!(engine.search("*보고서*", &options).unwrap().rows.len(), 2);
        assert_eq!(
            engine.search("보고서_??.pdf", &options).unwrap().rows.len(),
            2
        );
        assert_eq!(engine.search("*{{변수}}*", &options).unwrap().rows, vec![2]);
        assert!(
            engine
                .search("보고서_?.pdf", &options)
                .unwrap()
                .rows
                .is_empty()
        );
        assert_eq!(engine.search("*.pdf", &options).unwrap().rows.len(), 2);
    }
    #[test]
    fn regex_and_invalid_regex() {
        let engine = engine();
        let options = SearchOptions {
            mode: SearchMode::Regex,
            ..Default::default()
        };
        assert_eq!(
            engine
                .search(r"^보고서_\d{2}\.pdf$", &options)
                .unwrap()
                .rows,
            vec![1]
        );
        assert_eq!(
            engine.search(r"(?-i:PDF)$", &options).unwrap().rows,
            vec![0]
        );
        assert!(engine.search("[", &options).is_err());
    }
    #[test]
    fn plain_case_and_full_path_boundary() {
        let engine = engine();
        let mut options = SearchOptions::default();
        assert_eq!(engine.search("PDF", &options).unwrap().rows.len(), 2);
        options.case_sensitive = true;
        assert_eq!(engine.search("PDF", &options).unwrap().rows, vec![0]);
        options.match_path = true;
        assert_eq!(
            engine.search("자료\\보고서", &options).unwrap().rows.len(),
            2
        );
        assert_eq!(engine.search("\\", &options).unwrap().rows.len(), 4);
        options.match_path = false;
        assert!(engine.search("자료", &options).unwrap().rows.is_empty());
    }
    #[test]
    fn missing_metadata_always_last_and_folders_have_no_size() {
        let engine = engine();
        let mut options = SearchOptions {
            sort: SortField::Size,
            descending: false,
            ..Default::default()
        };
        assert_eq!(engine.search("", &options).unwrap().rows, vec![1, 0, 3, 2]);
        options.descending = true;
        assert_eq!(engine.search("", &options).unwrap().rows, vec![0, 1, 3, 2]);
        options.sort = SortField::Created;
        assert_eq!(engine.search("", &options).unwrap().rows, vec![1, 0, 3, 2]);
        options.sort = SortField::Modified;
        assert_eq!(engine.search("", &options).unwrap().rows, vec![0, 2, 3, 1]);
        options.kind = EntryKind::Folders;
        assert_eq!(engine.search("", &options).unwrap().rows, vec![3]);
    }
    #[test]
    fn path_search_can_span_separator_with_glob() {
        let engine = engine();
        let options = SearchOptions {
            mode: SearchMode::Wildcard,
            match_path: true,
            ..Default::default()
        };
        assert_eq!(
            engine
                .search("c:\\자료\\보고서_??.pdf", &options)
                .unwrap()
                .rows
                .len(),
            2
        );
        assert_eq!(
            engine.search("d:*{{변수}}*", &options).unwrap().rows,
            vec![2]
        );
    }

    #[test]
    fn broad_results_keep_the_same_order_after_sort_cache_warms() {
        let entries = (0..120_000)
            .map(|row| Entry {
                name: format!(
                    "{}_{:06}.txt",
                    if row % 20 == 0 { "Hit" } else { "other" },
                    119_999 - row
                ),
                parent: row % 3,
                size: Some(row as u64),
                created: None,
                modified: None,
                is_dir: false,
            })
            .collect();
        let engine = SearchEngine::new(Arc::new(Index {
            entries,
            folders: vec!["C:\\b".into(), "C:\\a".into(), "C:\\c".into()].into(),
            ..Default::default()
        }));
        for field in [SortField::Name, SortField::Path] {
            let mut options = SearchOptions {
                sort: field,
                descending: false,
                ..Default::default()
            };
            let direct = engine.search("hit", &options).unwrap().rows;
            assert_eq!(direct.len(), 6000);
            assert!(
                !engine
                    .sorted
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|c| c.field == field)
            );
            engine.search("", &options).unwrap();
            assert!(
                engine
                    .sorted
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|c| c.field == field)
            );
            assert_eq!(engine.search("hit", &options).unwrap().rows, direct);
            options.descending = true;
            assert_eq!(
                engine.search("hit", &options).unwrap().rows,
                direct.into_iter().rev().collect::<Vec<_>>()
            );
        }
    }
}
