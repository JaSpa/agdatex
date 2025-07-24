use std::ops::{Bound, Range, RangeBounds};

pub type Offset = usize;
pub type Span = Range<Offset>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SpanStr<'s> {
    str: &'s str,
    offset: Offset,
}

impl PartialEq<str> for SpanStr<'_> {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<SpanStr<'_>> for str {
    fn eq(&self, other: &SpanStr<'_>) -> bool {
        self == other.as_str()
    }
}

impl<'s> SpanStr<'s> {
    const EMPTY: Self = Self::new("");

    /// Returns a string with its span starting at byte position zero.
    pub const fn new(str: &'s str) -> Self {
        Self { str, offset: 0 }
    }

    pub const fn with_offset(str: &'s str, offset: Offset) -> Self {
        Self { str, offset }
    }

    pub fn span(self) -> Span {
        self.offset..self.offset + self.str.len()
    }

    pub fn as_str(self) -> &'s str {
        self.str
    }

    pub fn is_empty(self) -> bool {
        self.str.is_empty()
    }

    pub fn len(self) -> usize {
        self.str.len()
    }

    pub fn trim_start(self) -> Self {
        if let Some(idx) = self.str.find(|c: char| !c.is_whitespace()) {
            // SAFETY: `idx` is a valid index for `self.str` because it was returned by `str::find`.
            let (_, trimmed_start) = unsafe { self.split_at_unchecked(idx) };
            SpanStr {
                str: trimmed_start.str,
                offset: trimmed_start.offset,
            }
        } else {
            // The whole string consists of whitespace. Return an empty string located at the end.
            SpanStr {
                str: "",
                offset: self.offset + self.str.len(),
            }
        }
    }

    pub fn trim(self) -> Self {
        let SpanStr { str, offset } = self.trim_start();
        SpanStr {
            str: str.trim_end(),
            offset,
        }
    }

    pub fn strip_prefix(self, prefix: &str) -> Option<Self> {
        self.str.strip_prefix(prefix).map(|rest| SpanStr {
            str: rest,
            offset: self.offset + prefix.len(),
        })
    }

    pub fn split_inclusive_r(self, predicate: impl FnMut(char) -> bool) -> Option<(Self, Self)> {
        self.str.find(predicate).map(|index| {
            // SAFETY: `index` is a valid index because it was returned by `str.find(..)`.
            unsafe { self.split_at_unchecked(index) }
        })
    }

    /// # Safety
    ///
    /// `index` must be a valid index into [`Self::as_str()`].
    unsafe fn split_at_unchecked(self, index: usize) -> (Self, Self) {
        let str_a = unsafe { self.str.get_unchecked(..index) };
        let str_b = unsafe { self.str.get_unchecked(index..) };
        let a = SpanStr {
            str: str_a,
            offset: self.offset,
        };
        let b = SpanStr {
            str: str_b,
            offset: self.offset + index,
        };
        (a, b)
    }

    pub fn get(self, range: impl RangeBounds<usize>) -> Self {
        let start = range.start_bound().cloned();
        let end = range.end_bound().cloned();
        self.mk_offset_slice(start, &self.str[(start, end)])
    }

    /// # Safety
    ///
    /// `range` must be a valid range for [`Self::as_str()`].
    unsafe fn get_unchecked(self, range: impl RangeBounds<usize>) -> Self {
        let start = range.start_bound().cloned();
        let end = range.end_bound().cloned();

        // SAFETY: function safety contract.
        let str = unsafe { self.str.get_unchecked((start, end)) };

        self.mk_offset_slice(start, str)
    }

    fn mk_offset_slice<'a>(self, start: Bound<usize>, str: &'a str) -> SpanStr<'a> {
        let offset = match start {
            Bound::Included(n) => self.offset + n,
            Bound::Excluded(n) => self.offset + n + 1,
            Bound::Unbounded => self.offset,
        };
        SpanStr { str, offset }
    }

    pub fn lines(self) -> Lines<'s> {
        Lines(self)
    }
}

impl AsRef<str> for SpanStr<'_> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Debug for SpanStr<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if f.alternate() {
            f.debug_struct("Spanned")
                .field("str", &self.str)
                .field("offset", &self.offset)
                .finish()
        } else {
            self.str.fmt(f)?;
            write!(f, " ({:?})", self.span())
        }
    }
}

impl std::fmt::Display for SpanStr<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.str)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Lines<'s>(SpanStr<'s>);

impl<'s> Iterator for Lines<'s> {
    type Item = SpanStr<'s>;

    fn next(&mut self) -> Option<Self::Item> {
        let Lines(s) = *self;
        if s.is_empty() {
            None
        } else if let Some((ln, rest)) = s.str.split_once('\n') {
            *self = Lines(SpanStr {
                str: rest,
                offset: s.offset + ln.len() + 1,
            });
            Some(SpanStr {
                str: ln.strip_suffix('\r').unwrap_or(ln),
                offset: s.offset,
            })
        } else {
            self.0.str = "";
            Some(s)
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let min = if self.0.is_empty() { 0 } else { 1 };
        (min, None)
    }

    fn last(mut self) -> Option<Self::Item> {
        self.next_back()
    }
}

impl DoubleEndedIterator for Lines<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let Lines(SpanStr { mut str, offset }) = *self;

        if str.is_empty() {
            return None;
        }

        if let Some(str_no_nl) = str.strip_suffix('\n') {
            str = str_no_nl.strip_suffix('\r').unwrap_or(str_no_nl);
        }

        // We can't use `str::rsplit_once` here, because we want to keep the separator around in
        // the first segment.
        let (prev, line) = if let Some(nl_idx) = str.rfind('\n') {
            // SAFETY: the ranges are valid for `str` because the indices where produced by
            // `str.rfind('\n')`.
            unsafe {
                (
                    str.get_unchecked(..=nl_idx),
                    str.get_unchecked(nl_idx + 1..),
                )
            }
        } else {
            ("", str)
        };

        self.0.str = prev;
        Some(SpanStr {
            str: line,
            offset: offset + prev.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_empty() {
        let lines = Vec::from_iter(SpanStr::new("").lines());
        assert_eq!(lines, vec![] as Vec<SpanStr<'_>>);

        let rev_lines = Vec::from_iter(SpanStr::new("").lines().rev());
        assert_eq!(rev_lines, vec![] as Vec<SpanStr<'_>>);
    }

    #[test]
    fn lines_single_unterminated() {
        {
            const LINE: &str = "Hello, World!";
            let str = SpanStr::new(LINE);

            let lines = Vec::from_iter(str.lines());
            assert_eq!(lines, vec![SpanStr::with_offset(LINE, 0)]);

            let rev_lines = Vec::from_iter(str.lines().rev());
            assert_eq!(rev_lines, vec![SpanStr::with_offset(LINE, 0)]);
        }

        {
            const LINE: &str = "Hello, World!\r";
            let str = SpanStr::new(LINE);

            let lines = Vec::from_iter(str.lines());
            assert_eq!(lines, vec![SpanStr::with_offset(LINE, 0)]);

            let rev_lines = Vec::from_iter(str.lines().rev());
            assert_eq!(rev_lines, vec![SpanStr::with_offset(LINE, 0)]);
        }
    }

    #[test]
    fn lines_single_terminated() {
        const LINE: &str = "Hello, World!";

        {
            let str = format!("{LINE}\n");
            let str = SpanStr::new(&str);

            let lines = Vec::from_iter(str.lines());
            assert_eq!(lines, vec![SpanStr::with_offset(LINE, 0)]);

            let rev_lines = Vec::from_iter(str.lines().rev());
            assert_eq!(rev_lines, vec![SpanStr::with_offset(LINE, 0)]);
        }

        {
            let str = format!("{LINE}\r\n");
            let str = SpanStr::new(&str);

            let lines = Vec::from_iter(str.lines());
            assert_eq!(lines, vec![SpanStr::with_offset(LINE, 0)]);

            let rev_lines = Vec::from_iter(str.lines().rev());
            assert_eq!(rev_lines, vec![SpanStr::with_offset(LINE, 0)]);
        }
    }

    #[test]
    fn lines_multiline() {
        #[rustfmt::skip]
        let lines = [
            "01234567",
            "abcdefgh",
            "ABCDEFGH",
            "jklmnopq",
            "JKLMNOPQ"
        ];

        let lines_spanned = lines
            .iter()
            .enumerate()
            .map(|(i, ln)| SpanStr::with_offset(ln, i * 10))
            .collect::<Vec<_>>();
        let lines_spanned_rev = lines_spanned.iter().rev().copied().collect::<Vec<_>>();

        let str = lines.join("\r\n");
        assert_eq!(Vec::from_iter(SpanStr::new(&str).lines()), lines_spanned);
        assert_eq!(
            Vec::from_iter(SpanStr::new(&str).lines().rev()),
            lines_spanned_rev
        );

        let mut iter = SpanStr::new(&str).lines();
        assert_eq!(iter.next(), Some(lines_spanned[0]));
        assert_eq!(iter.next_back(), Some(lines_spanned_rev[0]));
        assert_eq!(iter.next(), Some(lines_spanned[1]));
        assert_eq!(iter.next_back(), Some(lines_spanned_rev[1]));
        assert_eq!(iter.next(), Some(lines_spanned[2]));
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next_back(), None);
    }

    #[test]
    fn split_inclusive_r() {
        let str = SpanStr::new("hello=bye");
        assert_eq!(str.split_inclusive_r(|c| c == ' '), None);
        assert_eq!(
            str.split_inclusive_r(|c| c == '='),
            Some((
                SpanStr::with_offset("hello", 0),
                SpanStr::with_offset("=bye", 5)
            ))
        );
    }
}
