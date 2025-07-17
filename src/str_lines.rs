//! This module provides an extension trait for [`str`] to add a version of [`str::lines()`] that
//! also includes the byte offset of the start of the line, similar to [`str::char_indices()`].

use std::iter::FusedIterator;

pub trait StrLines {
    fn line_indices(&self) -> IndexedLines<'_>;
}

impl StrLines for str {
    fn line_indices(&self) -> IndexedLines<'_> {
        IndexedLines {
            str: self,
            offset: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line<'s> {
    offset: usize,
    line: &'s str,
}

impl Line<'_> {
    pub fn offset(self) -> usize {
        self.offset
    }
}

impl AsRef<str> for Line<'_> {
    fn as_ref(&self) -> &str {
        self.line
    }
}

impl std::ops::Deref for Line<'_> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.line
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexedLines<'s> {
    str: &'s str,
    offset: usize,
}

impl<'s> Iterator for IndexedLines<'s> {
    type Item = Line<'s>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.str.is_empty() {
            None
        } else if let Some((ln, rest)) = self.str.split_once('\n') {
            let advance = ln.len();
            let line = Line {
                line: ln.strip_suffix('\r').unwrap_or(ln),
                offset: self.offset,
            };
            self.str = rest;
            self.offset += advance;
            Some(line)
        } else {
            Some(Line {
                line: std::mem::take(&mut self.str),
                offset: self.offset,
            })
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let min = if self.str.is_empty() { 0 } else { 1 };
        (min, None)
    }

    fn last(mut self) -> Option<Self::Item> {
        self.next_back()
    }
}

impl<'s> DoubleEndedIterator for IndexedLines<'s> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.str.is_empty() {
            return None;
        }

        if let Some(str) = self.str.strip_suffix('\n') {
            self.str = str.strip_suffix('\r').unwrap_or(str);
        }

        let idx = self.str.rfind('\n').unwrap_or(0);
        let (prev, ln) = self.str.split_at(idx);
        self.str = prev;
        Some(Line {
            line: ln,
            offset: self.offset + idx,
        })
    }
}

impl FusedIterator for IndexedLines<'_> {}
