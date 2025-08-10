use std::{
    fmt,
    io::{self, IoSlice},
    marker::PhantomData,
};

use crate::write_all_vectored;

#[repr(C)]
#[derive(Clone)]
pub struct SliceArray<'s, Init> {
    init: Init,
    last: IoSlice<'s>,
}

pub trait Nat: Sized {
    const VALUE: usize;

    type Bufs<'s>: Clone;

    fn snoc<'s>(ts: Self::Bufs<'s>, t: IoSlice<'s>) -> <S<Self> as Nat>::Bufs<'s> {
        SliceArray { init: ts, last: t }
    }

    fn mk_bufs<'a, 's>(bufs: &'a Self::Bufs<'s>) -> &'a [IoSlice<'s>] {
        let bufs_ptr = bufs as *const Self::Bufs<'s>;
        let slices_ptr = bufs_ptr as *const IoSlice<'s>;
        unsafe { std::slice::from_raw_parts(slices_ptr, Self::VALUE) }
    }

    fn mk_bufs_mut<'a, 's>(bufs: &'a mut Self::Bufs<'s>) -> &'a mut [IoSlice<'s>] {
        let bufs_ptr = bufs as *mut Self::Bufs<'s>;
        let slices_ptr = bufs_ptr as *mut IoSlice<'s>;
        unsafe { std::slice::from_raw_parts_mut(slices_ptr, Self::VALUE) }
    }
}

impl Nat for Z {
    const VALUE: usize = 0;

    type Bufs<'s> = ();
}

impl<N: Nat> Nat for S<N> {
    const VALUE: usize = N::VALUE + 1;

    type Bufs<'s> = SliceArray<'s, N::Bufs<'s>>;
}

pub struct Z;
pub struct S<N>(PhantomData<N>);

pub struct Ltx<'s, N: Nat> {
    slices: N::Bufs<'s>,
}

impl<N: Nat> Clone for Ltx<'_, N> {
    fn clone(&self) -> Self {
        Ltx {
            slices: self.slices.clone(),
        }
    }
}

impl<'s> Ltx<'s, Z> {
    pub const fn new() -> Self {
        Ltx { slices: () }
    }
}

impl<'s> Default for Ltx<'s, Z> {
    fn default() -> Self {
        Self::new()
    }
}

pub type Add2<N> = S<S<N>>;
pub type Add3<N> = S<Add2<N>>;
pub type Add5<N> = Add2<Add3<N>>;

pub struct Group {
    pub open: &'static str,
    pub close: &'static str,
}

impl Group {
    pub const TEX: Group = Group {
        open: "{",
        close: "}",
    };
    pub const OPT: Group = Group {
        open: "[",
        close: "]",
    };
}

impl<'s, N: Nat> Ltx<'s, N> {
    pub fn push(self, s: &'s str) -> Ltx<'s, S<N>> {
        Ltx {
            slices: N::snoc(self.slices, IoSlice::new(s.as_bytes())),
        }
    }

    pub fn ln(self) -> Ltx<'s, S<N>> {
        self.push("\n")
    }

    pub fn pctln(self) -> Ltx<'s, S<N>> {
        self.push("%\n")
    }

    #[cfg_attr(not(test), allow(unused))]
    pub fn par(self) -> Ltx<'s, Add2<N>> {
        self.ln().ln()
    }

    pub fn command(self, cmd: &'s str) -> Ltx<'s, Add2<N>> {
        self.push("\\").push(cmd)
    }

    pub fn group(self, inner: &'s str) -> Ltx<'s, Add3<N>> {
        self.with_group(Group::TEX, |ltx| ltx.push(inner))
    }

    pub fn opt(self, inner: &'s str) -> Ltx<'s, Add3<N>> {
        self.with_group(Group::OPT, |ltx| ltx.push(inner))
    }

    pub fn with_group<M: Nat>(
        self,
        group: Group,
        f: impl FnOnce(Ltx<'s, S<N>>) -> Ltx<'s, M>,
    ) -> Ltx<'s, S<M>> {
        f(self.push(group.open)).push(group.close)
    }

    pub fn begin(self, env: &'s str, arg: &'s str) -> Ltx<'s, impl Nat> {
        self.command("begin").group(env).opt(arg)
    }

    pub fn begin_(self, env: &'s str) -> Ltx<'s, Add5<N>> {
        self.command("begin").group(env)
    }

    pub fn end(self, env: &'s str) -> Ltx<'s, Add5<N>> {
        self.command("end").group(env)
    }

    #[cfg_attr(not(test), allow(unused))]
    pub fn with_env<M: Nat>(
        self,
        env: &'s str,
        f: impl FnOnce(Ltx<'s, Add5<N>>) -> Ltx<'s, M>,
    ) -> Ltx<'s, Add5<M>> {
        f(self.begin_(env)).end(env)
    }

    pub fn with_comment<M: Nat>(
        self,
        f: impl FnOnce(Ltx<'s, S<N>>) -> Ltx<'s, M>,
    ) -> Ltx<'s, impl Nat> {
        f(self.push("% ")).ln()
    }

    pub fn write(mut self, writer: impl io::Write) -> io::Result<()> {
        let bufs = N::mk_bufs_mut(&mut self.slices);
        write_all_vectored(writer, bufs)
    }

    fn iter(&self) -> impl Iterator<Item = &str> {
        N::mk_bufs(&self.slices).iter().map(move |slice| {
            // SAFETY: all `IoSlice`s are created from strings.
            unsafe { std::str::from_utf8_unchecked(slice) }
        })
    }

    pub fn write_fmt(self, writer: &mut dyn fmt::Write) -> fmt::Result {
        self.iter().try_for_each(|s| writer.write_str(s))
    }
}

impl<'s, N: Nat> fmt::Debug for Ltx<'s, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct Elems<'a, 's, N: Nat>(&'a Ltx<'s, N>);

        impl<N: Nat> fmt::Debug for Elems<'_, '_, N> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_list().entries(self.0.iter()).finish()
            }
        }

        f.debug_tuple("Ltx").field(&Elems(self)).finish()
    }
}

impl<N: Nat> fmt::Display for Ltx<'_, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.clone().write_fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write() {
        let expected_lines = [
            r"\documentclass[ngerman]{scrartcl}",
            r"\usepackage{blindtext}",
            r"",
            r"\begin{document}",
            r"Hi!",
            r"",
            r"\blindtext",
            r"\end{document}",
        ];
        let ltx = Ltx::new()
            .command("documentclass")
            .opt("ngerman")
            .group("scrartcl")
            .ln()
            .command("usepackage")
            .group("blindtext")
            .par()
            .with_env("document", |ltx| {
                ltx.ln().push("Hi!").par().command("blindtext").ln()
            })
            .ln();
        let actual = ltx.to_string();
        let actual_lines = actual.lines().collect::<Vec<_>>();

        assert_eq!(expected_lines.len(), actual_lines.len());
        for (i, (expected, actual)) in expected_lines.into_iter().zip(actual_lines).enumerate() {
            assert_eq!(expected, actual, "mismatch in line #{}", i + 1);
        }
    }
}
