use std::io::{IoSlice, Result, Write};

use sha2::digest::{FixedOutput, Output, Update};

pub struct HashWriter<D, W> {
    writer: W,
    digest: D,
}

impl<D, W> HashWriter<D, W> {
    pub fn new(writer: W) -> Self
    where
        D: Default,
    {
        HashWriter {
            writer,
            digest: D::default(),
        }
    }

    pub fn finalize(self) -> (Output<D>, W)
    where
        D: FixedOutput,
    {
        (self.digest.finalize_fixed(), self.writer)
    }
}

impl<D: Default, W> From<W> for HashWriter<D, W> {
    fn from(value: W) -> Self {
        Self::new(value)
    }
}

impl<D: Update, W: Write> Write for HashWriter<D, W> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let n = self.writer.write(buf)?;
        self.digest.update(&buf[..n]);
        Ok(n)
    }

    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.writer.write_all(buf)?;
        self.digest.update(buf);
        Ok(())
    }

    fn write_vectored(&mut self, bufs: &[IoSlice<'_>]) -> Result<usize> {
        let n = self.writer.write_vectored(bufs)?;
        let mut d_rem = n;
        for slice in bufs {
            if d_rem <= slice.len() {
                // SAFETY: we just checked that d_rem is in-bounds.
                self.digest.update(unsafe { slice.get_unchecked(..d_rem) });
                break;
            }
            self.digest.update(slice);
            d_rem -= slice.len();
        }
        Ok(n)
    }

    fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}
