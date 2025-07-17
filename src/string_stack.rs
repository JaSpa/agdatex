use std::iter::FusedIterator;

#[derive(Clone, Default)]
pub struct StringStack {
    data: String,
    lens: Vec<usize>,
}

impl StringStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.lens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lens.is_empty()
    }

    pub fn clear(&mut self) {
        self.data.clear();
        self.lens.clear();
    }

    pub fn push(&mut self, s: &str) {
        self.lens.push(self.data.len());
        self.data.push_str(s);
    }

    pub fn top(&self) -> Option<&str> {
        self.lens
            .last()
            .map(|&len| unsafe { self.data.get_unchecked(len..) })
    }

    pub fn top_mut(&mut self) -> Option<&mut str> {
        self.lens
            .last()
            .map(|&len| unsafe { self.data.get_unchecked_mut(len..) })
    }

    pub fn try_pop(&mut self) -> Option<PoppedStr> {
        self.lens.pop().map(|len| PoppedStr {
            stack: self,
            pop_len: len,
        })
    }

    pub fn pop(&mut self) -> PoppedStr {
        self.try_pop().expect("StringStack not empty")
    }

    pub fn iter(&self) -> Iter {
        Iter {
            data: &self.data,
            lens: self.lens.iter(),
        }
    }
}

impl<S: AsRef<str>> Extend<S> for StringStack {
    fn extend<T: IntoIterator<Item = S>>(&mut self, iter: T) {
        for item in iter {
            self.push(item.as_ref());
        }
    }
}

impl<S: AsRef<str>> FromIterator<S> for StringStack {
    fn from_iter<T: IntoIterator<Item = S>>(iter: T) -> Self {
        let mut stack = Self::new();
        stack.extend(iter);
        stack
    }
}

impl std::fmt::Debug for StringStack {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        struct Contents<'a>(&'a StringStack);

        impl std::fmt::Debug for Contents<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.debug_list().entries(self.0.iter()).finish()
            }
        }

        f.debug_tuple("StringStack").field(&Contents(self)).finish()
    }
}

#[derive(Debug, Clone)]
pub struct Iter<'a> {
    data: &'a str,
    lens: std::slice::Iter<'a, usize>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        let segment_start = *self.lens.next_back()?;
        Some(if let Some(&offset) = self.lens.as_slice().first() {
            let start = segment_start - offset;
            let segment = unsafe { self.data.get_unchecked(start..) };
            self.data = unsafe { self.data.get_unchecked(..start) };
            segment
        } else {
            self.data
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.lens.size_hint()
    }

    fn count(self) -> usize {
        self.lens.count()
    }

    fn last(mut self) -> Option<Self::Item> {
        self.next_back()
    }
}

impl DoubleEndedIterator for Iter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let offset = *self.lens.next()?;
        Some(if let Some(&end) = self.lens.as_slice().first() {
            let end = end - offset;
            let segment = unsafe { self.data.get_unchecked(..end) };
            self.data = unsafe { self.data.get_unchecked(end..) };
            segment
        } else {
            self.data
        })
    }
}

impl FusedIterator for Iter<'_> {}

impl ExactSizeIterator for Iter<'_> {
    fn len(&self) -> usize {
        self.lens.len()
    }
}

pub struct PoppedStr<'a> {
    stack: &'a mut StringStack,
    pop_len: usize,
}

impl PoppedStr<'_> {
    pub fn as_str(&self) -> &str {
        unsafe { self.stack.data.get_unchecked(self.pop_len..) }
    }

    pub fn as_str_mut(&mut self) -> &mut str {
        unsafe { self.stack.data.get_unchecked_mut(self.pop_len..) }
    }
}

impl AsRef<str> for PoppedStr<'_> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsMut<str> for PoppedStr<'_> {
    fn as_mut(&mut self) -> &mut str {
        self.as_str_mut()
    }
}

impl std::ops::Deref for PoppedStr<'_> {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl std::ops::DerefMut for PoppedStr<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.as_str_mut()
    }
}

impl Drop for PoppedStr<'_> {
    fn drop(&mut self) {
        unsafe { self.stack.data.as_mut_vec().set_len(self.pop_len) }
    }
}

impl std::fmt::Display for PoppedStr<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}

impl std::fmt::Debug for PoppedStr<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_tuple("PoppedStr").field(&self.as_str()).finish()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const S1: &str = "hi";
    const S2: &str = "there";
    const S3: &str = "y'all good!?";

    macro_rules! strs {
        () => {
            [S1, S2, S3, S1, S2, S3]
        };
        (rev) => {
            [S3, S2, S1, S3, S2, S1]
        };
    }

    #[test]
    fn try_pop_empty() {
        assert!(StringStack::new().try_pop().is_none());
    }

    #[test]
    fn push_top_pop() {
        let mut stack = StringStack::new();
        stack.push(S1);
        stack.push(S2);
        stack.push(S3);

        assert_eq!(stack.top(), Some(S3));
        assert_eq!(stack.pop().as_str(), S3);

        assert_eq!(stack.top(), Some(S2));
        assert_eq!(stack.pop().as_str(), S2);

        assert_eq!(stack.top(), Some(S1));
        assert_eq!(stack.pop().as_str(), S1);

        assert_eq!(stack.top(), None);
        assert!(stack.try_pop().is_none());
    }

    #[test]
    fn iter_forwards() {
        let stack = StringStack::from_iter(strs!());
        let items = Vec::from_iter(stack.iter());
        assert_eq!(items, strs!(rev));
    }

    #[test]
    fn iter_backwards() {
        let stack = StringStack::from_iter(strs!());
        let items = Vec::from_iter(stack.iter().rev());
        assert_eq!(items, strs!());
    }

    #[test]
    fn iter_mixed() {
        let stack = StringStack::from_iter(strs!());
        let mut iter = stack.iter();

        let mut sink_fwd = Vec::new();
        let mut sink_bwd = Vec::new();

        loop {
            let Some(fwd) = iter.next() else {
                break;
            };
            sink_fwd.push(fwd);

            let Some(bwd) = iter.next_back() else {
                break;
            };
            sink_bwd.push(bwd);
        }

        assert_eq!(sink_fwd, [S3, S2, S1]);
        assert_eq!(sink_bwd, [S1, S2, S3]);
    }

    #[test]
    fn iter_mixed2() {
        let stack = StringStack::from_iter(strs!());
        let mut iter = stack.iter();

        let mut sink_fwd = Vec::new();
        let mut sink_bwd = Vec::new();

        loop {
            let Some(bwd) = iter.next_back() else {
                break;
            };
            sink_bwd.push(bwd);

            let Some(fwd) = iter.next() else {
                break;
            };
            sink_fwd.push(fwd);
        }

        assert_eq!(sink_fwd, [S3, S2, S1]);
        assert_eq!(sink_bwd, [S1, S2, S3]);
    }

    #[test]
    fn len_and_is_empty() {
        let mut stack = StringStack::new();

        assert!(stack.is_empty());
        assert_eq!(stack.len(), 0);

        stack.push(S1);

        assert!(!stack.is_empty());
        assert_eq!(stack.len(), 1);

        stack.pop();
        assert!(stack.is_empty());
        assert_eq!(stack.len(), 0);

        stack.extend(strs!());
        assert!(!stack.is_empty());
        assert_eq!(stack.len(), strs!().len());
    }
}
