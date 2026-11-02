//! A single compiled highlighter. Only visible cells ask for UTF-16 spans.
use crate::engine::SearchMode;
use regex::{Regex, RegexBuilder};
use std::ops::Range;

#[derive(Default)]
pub struct Highlighter(Option<Regex>);
impl Highlighter {
    pub fn cell_spans(
        &self,
        name: &str,
        parent: &str,
        match_path: bool,
        column: usize,
    ) -> Vec<Range<usize>> {
        if !match_path {
            return if column == 0 {
                self.spans(name)
            } else {
                Vec::new()
            };
        }
        let separator = if parent.ends_with('\\') { "" } else { "\\" };
        let path = format!("{parent}{separator}{name}");
        let offset = if column == 0 {
            parent.encode_utf16().count() + separator.len()
        } else {
            0
        };
        let length = if column == 0 {
            name.encode_utf16().count()
        } else {
            parent.encode_utf16().count()
        };
        self.spans(&path)
            .into_iter()
            .filter_map(|span| {
                let start = span.start.max(offset);
                let end = span.end.min(offset + length);
                (start < end).then(|| start - offset..end - offset)
            })
            .collect()
    }
    pub fn new(query: &str, mode: SearchMode, case_sensitive: bool) -> Self {
        let expression = match mode {
            SearchMode::Regex => query.to_owned(),
            SearchMode::Plain => regex::escape(query),
            SearchMode::Wildcard => query
                .split(['*', '?'])
                .filter(|s| !s.is_empty())
                .map(regex::escape)
                .collect::<Vec<_>>()
                .join("|"),
        };
        Self(
            (!expression.is_empty())
                .then(|| {
                    RegexBuilder::new(&expression)
                        .case_insensitive(!case_sensitive)
                        .size_limit(8 * 1024 * 1024)
                        .build()
                        .ok()
                })
                .flatten(),
        )
    }
    pub fn spans(&self, text: &str) -> Vec<Range<usize>> {
        let Some(regex) = &self.0 else {
            return Vec::new();
        };
        regex
            .find_iter(text)
            .filter(|m| !m.is_empty())
            .map(|m| {
                let start = text[..m.start()].encode_utf16().count();
                start..start + m.as_str().encode_utf16().count()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn korean_wildcard_highlights_literals_without_coloring_entire_filename() {
        let h = Highlighter::new("*보고서*.pdf", SearchMode::Wildcard, false);
        assert_eq!(h.spans("📄월간보고서.PDF"), vec![4..7, 7..11]);
        assert!(
            Highlighter::new("***?", SearchMode::Wildcard, false)
                .spans("file")
                .is_empty()
        );
    }
    #[test]
    fn regex_case_and_repeated_matches_keep_utf16_boundaries() {
        let h = Highlighter::new("보고서|\\d+", SearchMode::Regex, false);
        assert_eq!(h.spans("😀보고서12보고서"), vec![2..5, 5..7, 7..10]);
        assert_eq!(
            Highlighter::new("a", SearchMode::Plain, false).spans("Aa"),
            vec![0..1, 1..2]
        );
        assert_eq!(
            Highlighter::new("a", SearchMode::Plain, true).spans("Aa"),
            vec![1..2]
        );
        assert!(
            Highlighter::new("[", SearchMode::Regex, false)
                .spans("file")
                .is_empty()
        );
        assert!(
            Highlighter::new("^", SearchMode::Regex, false)
                .spans("file")
                .is_empty()
        );
    }
    #[test]
    fn full_path_query_can_highlight_across_columns_and_drive_root_separator() {
        let h = Highlighter::new(r"보고서\월간", SearchMode::Plain, false);
        assert_eq!(h.cell_spans("월간.pdf", r"C:\보고서", true, 0), vec![0..2]);
        assert_eq!(h.cell_spans("월간.pdf", r"C:\보고서", true, 1), vec![3..6]);
        let h = Highlighter::new(r"C:\월간", SearchMode::Plain, false);
        assert_eq!(h.cell_spans("월간.pdf", "C:\\", true, 0), vec![0..2]);
    }
}
