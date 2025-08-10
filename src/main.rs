use std::{
    borrow::Cow,
    cell::LazyCell,
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::{ErrorKind, IoSlice, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, ExitStatus},
};

use base64::prelude::*;
use clap::Parser;
use color_eyre::{
    Result, Section, SectionExt,
    eyre::{Context, eyre},
};
use fs_at::OpenOptions as OpenOptionsAt;
use ltx_write::Ltx;
use nix::errno::Errno;
use sha2::{
    Digest, Sha256,
    digest::{OutputSizeUser, generic_array::GenericArray},
};
use tempdir::TempDir;
use translation::Translator;

mod ltx_write;
mod span_str;
mod string_stack;
mod translation;

#[derive(Clone, Copy)]
struct VerbOutput {
    enabled: bool,
}

impl VerbOutput {
    fn if_enabled(self, f: impl FnOnce()) {
        if self.enabled {
            f()
        }
    }
}

macro_rules! verb {
    ($v:expr, $($arg:tt)*) => {
        VerbOutput::if_enabled($v, || eprintln!($($arg)*))
    };
}

#[derive(Debug, Parser)]
struct Args {
    /// Output directory for Agda's LaTeX backend; forwarded as `--latex-dir`.
    #[arg(short, long = "outputdir", default_value = "latex")]
    output_dir: PathBuf,

    /// Temporary directory to copy the project root to. [default: fresh system-dependent
    /// directory]
    #[arg(short, long = "tempdir")]
    temp_dir: Option<PathBuf>,

    /// Enable verbose output.
    #[arg(short, long)]
    verbose: bool,

    /// Clear caches to force a rebuild of all modules.
    #[arg(short, long)]
    clear: bool,

    /// Enable fast Agda to LaTeX compilation; passes the `--only-scope-checking` flag to Agda.
    #[arg(long, alias = "only-scope-checking")]
    fast: bool,

    /// Paths to annotated .agda files.
    sources: Vec<PathBuf>,
}

fn normalize_source(path: impl AsRef<Path>, root: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    let root = root.as_ref();
    let can_path = path.canonicalize()?;
    let rel_path = can_path.strip_prefix(root).map_err(|_| {
        eyre!(
            "input {} not a subpath of project root {}",
            path.display(),
            root.display()
        )
    })?;
    Ok(rel_path.to_path_buf())
}

fn translate_stdin(_args: Args) -> Result<ExitCode> {
    let mut src = String::new();
    std::io::stdin().read_to_string(&mut src)?;
    let mut translated = Vec::new();
    let mut macros = Vec::new();
    let mut errors = false;

    Translator::default().run(
        &src,
        &mut translated,
        |diag| {
            errors = true;
            diag.to_report().eprint(NamedSource {
                name: "«stdin»",
                source: src.as_str().into(),
            })
        },
        |macro_| {
            macros.push(macro_.to_owned());
            Ok(())
        },
    )?;

    if errors {
        return Ok(ExitCode::FAILURE);
    }

    // Write the translated source to stdout.
    std::io::stdout().write_all(&translated)?;

    // Print the macros to stderr.
    for macro_ in macros {
        eprint!("{macro_}");
    }

    Ok(ExitCode::SUCCESS)
}

fn main() -> Result<ExitCode> {
    color_eyre::install().unwrap();

    let args = Args::parse();
    let verb = VerbOutput {
        enabled: args.verbose,
    };

    if args.sources.is_empty() {
        return translate_stdin(args);
    }
    verb!(verb, "Command line options: {args:#?}");

    let root = std::env::current_dir()?.canonicalize()?;
    let sources = args
        .sources
        .into_iter()
        .map(|src| normalize_source(src, &root))
        .collect::<Result<Vec<_>>>()?;
    verb!(verb, "Canonicalized sources: {sources:#?}");

    let mut tmp_dir: Option<TempDir> = None;
    let resolved_args = Agdatex {
        verb,
        fast_compile: args.fast,
        output_dir: args.output_dir,
        temp_dir: if let Some(tmp) = args.temp_dir {
            tmp
        } else {
            let tmp = TempDir::new("agdatex")?;
            let path = tmp.path().to_path_buf();
            tmp_dir = Some(tmp);
            path
        },
        state: State::default(),
    };

    verb!(
        verb,
        "Temporary directory: {}",
        resolved_args.temp_dir.display()
    );

    resolved_args.run(sources)?;

    // Make sure the temporary directory is dropped at the very end.
    std::mem::drop(tmp_dir);
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sha256Digest(GenericArray<u8, <Sha256 as OutputSizeUser>::OutputSize>);

#[derive(Debug)]
enum Sha256DecodeError {
    InvalidDigestLength,
    #[allow(dead_code)]
    DecodeError(base64::DecodeError),
}

impl From<base64::DecodeError> for Sha256DecodeError {
    fn from(error: base64::DecodeError) -> Self {
        Self::DecodeError(error)
    }
}

impl From<base64::DecodeSliceError> for Sha256DecodeError {
    fn from(error: base64::DecodeSliceError) -> Self {
        use base64::DecodeSliceError::*;
        match error {
            DecodeError(decode_error) => decode_error.into(),
            OutputSliceTooSmall => Sha256DecodeError::InvalidDigestLength,
        }
    }
}

impl Sha256Digest {
    /// The buffer size necessary to encode a sha356 digest using base64.
    const SHA256_ENCODED_SIZE: usize = 44;

    /// The [`base64::Engine`] to use for en- & decoding.
    const CODING_ENGINE: base64::engine::GeneralPurpose = BASE64_STANDARD_NO_PAD;

    fn with_encoding<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> R {
        let mut buf = [0; Self::SHA256_ENCODED_SIZE];
        let encoded_length = Self::CODING_ENGINE
            .encode_slice(self.0, &mut buf)
            .expect("miscalculated buffer size");
        f(&mut buf[..encoded_length])
    }

    fn decode(base64_bytes: &[u8]) -> Result<Self, Sha256DecodeError> {
        let mut sha256_buf = GenericArray::default();
        let n = Self::CODING_ENGINE.decode_slice(base64_bytes, &mut sha256_buf)?;
        (n == sha256_buf.len())
            .then_some(Sha256Digest(sha256_buf))
            .ok_or(Sha256DecodeError::InvalidDigestLength)
    }
}

#[derive(PartialEq, Eq)]
struct SourceHash {
    digest: Sha256Digest,
}

#[derive(Debug)]
enum SourceHashReadError {
    CantParse,
    #[allow(dead_code)]
    IOError(std::io::Error),
    #[allow(dead_code)]
    DigestDecodeError(Sha256DecodeError),
}

impl From<std::io::Error> for SourceHashReadError {
    fn from(error: std::io::Error) -> Self {
        Self::IOError(error)
    }
}

impl From<Sha256DecodeError> for SourceHashReadError {
    fn from(error: Sha256DecodeError) -> Self {
        Self::DigestDecodeError(error)
    }
}

impl SourceHash {
    const PREFIX: &str = "% SOURCE-HASH=";
    const HASH_LINE_CAP: usize =
        (Self::PREFIX.len() + Sha256Digest::SHA256_ENCODED_SIZE + 1).next_power_of_two();

    fn new(s: &str) -> Self {
        SourceHash {
            digest: Sha256Digest(Sha256::digest(s)),
        }
    }

    fn write(&self, writer: impl std::io::Write) -> std::io::Result<()> {
        self.digest.with_encoding(|ascii_digest| {
            let mut slices = [
                IoSlice::new(Self::PREFIX.as_bytes()),
                IoSlice::new(ascii_digest),
                IoSlice::new(b"\n\n"),
            ];
            write_all_vectored(writer, &mut slices)
        })
    }

    fn try_read(mut reader: impl std::io::Read) -> Result<Self, SourceHashReadError> {
        let mut buffer = [0u8; Self::HASH_LINE_CAP];
        let mut filled = 0;

        let nl_idx = loop {
            // Read more data into buffer.
            //
            // SAFETY: `filled` is always less than `Self::HASH_LINE_CAP`.
            let slice_to_fill = unsafe { buffer.get_unchecked_mut(filled..) };
            let bytes_read = loop {
                match reader.read(slice_to_fill) {
                    // Reached EOF before reading the source hash.
                    Ok(0) => return Err(SourceHashReadError::CantParse),
                    // Filled the buffer with additional data.
                    Ok(n) => break n.min(slice_to_fill.len()),
                    // Repeat if interrupted.
                    Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                    // Abort on any other I/O errors.
                    Err(err) => return Err(err.into()),
                }
            };

            // Check if the additional data contains the newline byte.
            //
            // SAFETY: `bytes_read` is less than or equal to `slice_to_fill.len()`.
            let additional_bytes = unsafe { slice_to_fill.get_unchecked(..bytes_read) };
            if let Some(nl_idx) = memchr::memchr(b'\n', additional_bytes) {
                // Adjust nl_idx with the data already read in previous iterations.
                break filled + nl_idx;
            }

            // Otherwise advance `filled` by `bytes_read`. Abort, if the whole buffer was filled
            // without a newline byte.
            filled += bytes_read;
            if filled == buffer.len() {
                return Err(SourceHashReadError::CantParse);
            }
        };

        // Extract the first line from `buffer`.
        //
        // SAFETY: `nl_idx` is less than `buffer.len().
        let first_line = unsafe { buffer.get_unchecked(..nl_idx) };
        // Try to strip the `PREFIX` to get to the actual hash.
        let encoded_hash = first_line
            .strip_prefix(Self::PREFIX.as_bytes())
            .ok_or(SourceHashReadError::CantParse)?;
        // Trim any ascii-whitespace bytes from the end.
        let last_hash_byte = encoded_hash
            .iter()
            .rposition(|b| !b.is_ascii_whitespace())
            .ok_or(SourceHashReadError::CantParse)?;
        // SAFETY: `last_hash_byte` is less than `encoded_hash.len()` because it was returned by
        // `encoded_hash.iter().rposition()`.
        let extracted_hash = unsafe { encoded_hash.get_unchecked(..=last_hash_byte) };

        // We extracted the hash, now try to decode the base64 representation.
        Ok(SourceHash {
            digest: Sha256Digest::decode(extracted_hash)?,
        })
    }
}

impl std::fmt::Display for SourceHash {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.digest
            .with_encoding(|base64_digest| f.write_str(&String::from_utf8_lossy(base64_digest)))
    }
}

struct Agdatex {
    output_dir: PathBuf,
    temp_dir: PathBuf,
    verb: VerbOutput,
    state: State,
    fast_compile: bool,
}

#[derive(Default)]
struct State {
    source_buffer: String,
    /// Paths to translated items that need to be (re)compiled.
    translated_item_paths: Vec<PathBuf>,
    translator: Translator,
    diagnostics_count: u32,
    cur_assemble_file: Option<File>,
}

#[derive(Debug, Clone, Copy)]
enum FileAction {
    Translate,
    Copy,
}

impl FileAction {
    fn description(self) -> &'static str {
        match self {
            FileAction::Translate => "TRANS",
            FileAction::Copy => "COPY",
        }
    }

    fn adjust_target_file_name(self, source: &Path) -> Cow<'_, Path> {
        match self {
            FileAction::Copy => source.into(),
            FileAction::Translate => {
                let mut p = source.to_owned();
                p.set_extension(EXT_TRANSLATED);
                p.into()
            }
        }
    }
}

const EXT_COMPILED: &str = "tex";
const EXT_TRANSLATED: &str = "lagda.tex";

#[derive(Debug, Clone, Copy)]
enum Item<'a> {
    UserInput {
        path: &'a Path,
    },
    ChildItem {
        name: &'a OsStr,
        parent_fd_source: &'a File,
        parent_fd_target: &'a File,
        parent_item: &'a Item<'a>,
    },
}

impl Item<'_> {
    fn file_name(&self) -> Option<&OsStr> {
        match self {
            Item::UserInput { path } => path.file_name(),
            Item::ChildItem { name, .. } => Some(name),
        }
    }

    fn file_action_impl(&self) -> Option<FileAction> {
        let name = Path::new(self.file_name()?);
        let ext1 = name.extension()?;
        let ext2 = || name.file_stem().map(Path::new).and_then(Path::extension);

        const EXT_AGDA: &str = "agda";
        const EXT_LAGDA: &str = "lagda";

        ext1.eq_ignore_ascii_case(EXT_AGDA)
            .then_some(FileAction::Translate)
            .or_else(|| {
                ext1.eq_ignore_ascii_case(EXT_LAGDA)
                    .then_some(FileAction::Copy)
            })
            .or_else(|| {
                ext2()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case(EXT_LAGDA))
                    .then_some(FileAction::Copy)
            })
    }

    fn file_action(&self) -> Result<Option<FileAction>> {
        let action = self.file_action_impl();
        if action.is_none()
            && let Item::UserInput { path } = self
        {
            Err(eyre!(
                "I don't know what to do with input file `{}'",
                path.display()
            ))
        } else {
            Ok(action)
        }
    }

    fn open_source(&self) -> Result<Option<File>> {
        Ok(match self {
            Item::UserInput { path } => Some(File::open(path)?),
            Item::ChildItem {
                parent_fd_source,
                name,
                ..
            } => {
                let res = OpenOptionsAt::default()
                    .read(true)
                    .follow(false)
                    .open_at(parent_fd_source, name);
                match res {
                    Ok(fd) => Some(fd),
                    Err(err) => {
                        // Have to go this way until ErrorKind::FilesystemLoop is stabilized.
                        if err.raw_os_error().map(Errno::from_raw) == Some(Errno::ELOOP) {
                            None
                        } else {
                            return Err(err.into());
                        }
                    }
                }
            }
        })
    }

    fn create_target_dir(&self, base: impl AsRef<Path>) -> Result<File> {
        Ok(match self {
            Item::UserInput { path } => {
                // There is no truly race-free way to create the directory and open it. Don't go
                // overboard here.
                let target_path = base.as_ref().join(path);
                std::fs::create_dir_all(&target_path)?;
                File::open(target_path)?
            }
            Item::ChildItem {
                name,
                parent_fd_target,
                ..
            } => OpenOptionsAt::default()
                .create(true)
                .mkdir_at(parent_fd_target, name)?,
        })
    }

    fn open_translation_target_file(&self, base: impl AsRef<Path>) -> Result<File> {
        let adjust_ext = |mut p: PathBuf| {
            p.set_extension(EXT_COMPILED);
            p
        };
        Ok(match self {
            Item::UserInput { path } => {
                let target_path = adjust_ext(base.as_ref().join(path));
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(target_path)?
            }
            Item::ChildItem {
                name,
                parent_fd_target,
                ..
            } => {
                let target_name = adjust_ext(name.into());
                OpenOptionsAt::default()
                    .write(fs_at::OpenOptionsWriteMode::Write)
                    .create(true)
                    .truncate(true)
                    .open_at(parent_fd_target, target_name)?
            }
        })
    }

    fn push_path(&self, path_buf: &mut PathBuf) {
        // Traverse the item tree summing the needed additional capacity before pushing each
        // segment.
        fn push_with_cap(item: &Item, path_buf: &mut PathBuf, additional_cap: usize) {
            match item {
                Item::UserInput { path } => {
                    let additional_cap = additional_cap + 1 + path.as_os_str().len();
                    path_buf.reserve(additional_cap);
                    path_buf.push(path)
                }
                Item::ChildItem {
                    name, parent_item, ..
                } => {
                    let additional_cap = additional_cap + 1 + name.len();
                    push_with_cap(parent_item, path_buf, additional_cap);
                    path_buf.push(name);
                }
            }
        }

        push_with_cap(self, path_buf, 0);
    }

    fn to_path_buf(self) -> PathBuf {
        let mut buf = PathBuf::new();
        self.push_path(&mut buf);
        buf
    }
}

impl Agdatex {
    fn run(mut self, sources: Vec<PathBuf>) -> Result<()> {
        // Ensure the output directory exists.
        std::fs::create_dir_all(&self.output_dir)?;

        // Translate all the inputs recursively.
        for input_path in sources {
            self.translate_item(Item::UserInput { path: &input_path })?;
            self.state.cur_assemble_file = None;
        }

        // Build the `--latex-dir=..` argument once.
        let mut latex_dir_arg = OsString::from("--latex-dir=");
        latex_dir_arg.push(std::path::absolute(&self.output_dir)?);

        // Compile all the translated files to LaTeX.
        for translated in self.state.translated_item_paths {
            verb!(self.verb, "COMPL {}", translated.display());
            Command::new("agda")
                .current_dir(&self.temp_dir)
                .args(self.fast_compile.then_some("--only-scope-checking"))
                .arg("--latex")
                .arg(&latex_dir_arg)
                .arg(&translated)
                .expect_run()?;
        }

        Ok(())
    }

    fn read_compiled_source_hash(&self, item: &Item) -> Result<SourceHash, SourceHashReadError> {
        // Assemble the path where the item will be placed.
        let mut target_path = self.output_dir.clone();
        item.push_path(&mut target_path);
        target_path.with_extension(EXT_COMPILED);

        // Extract the source hash from the previously compiled file.
        SourceHash::try_read(File::open(target_path)?)
    }

    fn maybe_push_translated_path(
        &mut self,
        item: &Item,
        source_hash: &SourceHash,
        translated: PathBuf,
    ) {
        match self.read_compiled_source_hash(item) {
            Ok(compiled_hash) if *source_hash == compiled_hash => {
                verb!(
                    self.verb,
                    "CACHE HIT {} ({source_hash})",
                    translated.display()
                );
                return;
            }
            Ok(compiled_hash) => {
                verb!(
                    self.verb,
                    "CACHE MISMATCH {}\n  old: {compiled_hash}\n  new: {source_hash}",
                    translated.display()
                );
            }
            Err(error) => {
                verb!(
                    self.verb,
                    "CACHE MISMATCH {} ({error:#?})",
                    translated.display()
                );
            }
        };

        // At this point we are sure to compile this item!
        self.state.translated_item_paths.push(translated);
    }

    fn translate_item(&mut self, item: Item) -> Result<()> {
        let Some(fd_item) = item.open_source()? else {
            return Ok(());
        };

        if fd_item.metadata()?.file_type().is_dir() {
            self.translate_dir(item, fd_item)?;
        } else {
            self.translate_file(item, fd_item)?;
        }

        Ok(())
    }

    fn translate_dir(&mut self, parent_item: Item, fd: File) -> Result<()> {
        let mut item_path_display = String::new();
        self.verb.if_enabled(|| {
            item_path_display = parent_item.to_path_buf().display().to_string();
        });
        verb!(self.verb, "ENTER {item_path_display}");

        // Create the corresponding directory in the target directory.
        let target_dir_fd = parent_item.create_target_dir(&self.temp_dir)?;

        // If we are translating a directory we have to create an assembly file if there is not
        // one already.
        if self.state.cur_assemble_file.is_none() {
            let mut path = self.output_dir.clone();
            parent_item.push_path(&mut path);
            path.set_extension("tex");
            self.state.cur_assemble_file = Some(
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(path)?,
            );
        }

        // We need access to `fd` in the loop body. `fs_at::read_dir` wants a &mut to the directory
        // fd. To avoid any unsafe shennanigans we pass a clone of the file descriptor to
        // `read_dir`.
        for entry_or_err in fs_at::read_dir(&mut fd.try_clone()?)? {
            let entry = entry_or_err?;
            let name = entry.name();

            // Skip over hidden directories.
            if name.has_prefix(".") {
                continue;
            }

            self.translate_item(Item::ChildItem {
                name,
                parent_fd_source: &fd,
                parent_fd_target: &target_dir_fd,
                parent_item: &parent_item,
            })?;
        }

        verb!(self.verb, "LEAVE {item_path_display}");
        Ok(())
    }

    fn translate_file(&mut self, item: Item, mut file: File) -> Result<()> {
        // Determine what to do with this file based on the extension.
        let Some(action) = item.file_action()? else {
            verb!(self.verb, "SKIP {}", item.to_path_buf().display());
            return Ok(());
        };

        let item_path = item.to_path_buf();
        let item_target_path = action.adjust_target_file_name(&item_path);
        let full_target_path = self.temp_dir.join(&item_target_path);
        verb!(
            self.verb,
            "{} {} ({})",
            action.description(),
            item_path.display(),
            full_target_path.display()
        );

        match action {
            // A .agda file. Translate annotations into LaTeX macro definitions.
            FileAction::Translate => {
                // Read in source.
                self.state.source_buffer.clear();
                file.read_to_string(&mut self.state.source_buffer)?;

                // Open target file.
                let out_file = item.open_translation_target_file(&self.temp_dir)?;
                // Write the source hash for future comparison.
                let source_hash = SourceHash::new(&self.state.source_buffer);
                source_hash.write(&out_file)?;

                // Translate source.
                //
                // There is nothing to gain with trying to avoid the `into_owned` call here: at
                // this point we know that the `Cow` must wrap an allocated value.
                let item_target_path = item_target_path.into_owned();
                let should_compile = self.translate_src(item_path, out_file)?;
                if should_compile {
                    self.maybe_push_translated_path(&item, &source_hash, item_target_path);
                }
            }

            // What is the best way to copy the source file untranslated to the temporary
            // directory?
            //
            // * `std::fs::copy` copies files using OS-specific optimized syscalls. It does not
            //   support any `_at` variants.
            //
            // * `std::io::copy` does a user-land copy by reading the file in chunks and writing it
            //   to the target file. Using `fs_at` we can emulate FD-relative copies.
            //
            // * `libc::linkat` allows FD-relative hard-link creation. But hardlinking may fail if
            //   our `temp_dir` is on another device.
            //
            // For now, we ignore FD-relativity and use `std::fs::copy.
            FileAction::Copy => {
                std::fs::copy(&item_path, &*full_target_path)?;
            }
        }

        Ok(())
    }

    /// Translate the current [`source_buffer`][State::source_buffer] and write the Literate Agda
    /// code to `out_file`.
    ///
    /// This function returns `true` if the translation recorded any macros. If no macros were
    /// encoutered there is no need to compile the file .lagda file to LaTeX.
    ///
    /// If there is an open umbrella file ([`State::cur_assemble_file`]) and the translation
    /// encountered macro definitions this function will add a suitable `\input{..}` line.
    ///
    /// # Errors
    ///
    /// This function will return an error if any I/O errors are encoutered while writing to
    /// `out_file` or when printing diagnostics.
    fn translate_src(&mut self, item_path: PathBuf, out_file: File) -> Result<bool> {
        let pp_path = LazyCell::new(|| item_path.display().to_string());
        let mut has_macro = false;

        // Run the translator.
        self.state.translator.run(
            &self.state.source_buffer,
            out_file,
            |diag| {
                self.state.diagnostics_count += 1;
                diag.to_report().print(NamedSource {
                    name: pp_path.as_str(),
                    source: self.state.source_buffer.as_str().into(),
                })
            },
            |macro_| {
                has_macro = true;
                if let Some(ref file) = self.state.cur_assemble_file {
                    macro_.to_ltx_comment().write(file)?;
                }
                Ok(())
            },
        )?;

        if has_macro && let Some(ref file) = self.state.cur_assemble_file {
            let mut tex_path = item_path;
            tex_path.set_extension(EXT_COMPILED);
            Ltx::new()
                .command("input")
                .group(&tex_path.as_os_str().to_string_lossy())
                .ln()
                .ln()
                .write(file)?;
        }

        // The file has to be compiled to LaTeX if it contained any macros.
        Ok(has_macro)
    }
}

struct NamedSource<'a> {
    name: &'a str,
    source: ariadne::Source<&'a str>,
}

impl<'a> ariadne::Cache<()> for NamedSource<'a> {
    type Storage = &'a str;

    fn fetch(&mut self, id: &()) -> Result<&ariadne::Source<Self::Storage>, impl std::fmt::Debug> {
        self.source.fetch(id)
    }

    fn display<'x>(&self, _id: &'x ()) -> Option<impl std::fmt::Display + 'x> {
        Some(self.name.to_owned())
    }
}

trait CommandSuccess {
    fn expect_success<R>(
        &self,
        result: std::io::Result<R>,
        get_status: impl FnOnce(&R) -> ExitStatus,
        customize_error: impl FnOnce(color_eyre::Report, R) -> color_eyre::Report,
    ) -> Result<R>;

    fn expect_run(&mut self) -> Result<()>;
}

impl CommandSuccess for Command {
    fn expect_success<R>(
        &self,
        result: std::io::Result<R>,
        get_status: impl FnOnce(&R) -> ExitStatus,
        customize_error: impl FnOnce(color_eyre::Report, R) -> color_eyre::Report,
    ) -> Result<R> {
        let cmd_section = |cmd: &Command| format!("{cmd:?}").header("Command");
        let result = result
            .wrap_err("process invocation failed")
            .with_section(|| cmd_section(self))?;

        let status = get_status(&result);
        if status.success() {
            return Ok(result);
        }

        let report = eyre!(
            "command {} failed with exit code {}",
            PseudoEscape(self.get_program().to_string_lossy()),
            status
        )
        .section(cmd_section(self));
        Err(customize_error(report, result))
    }

    fn expect_run(&mut self) -> Result<()> {
        let result = self.spawn().and_then(|mut child| child.wait());
        self.expect_success(result, |exit| *exit, |report, _| report)?;
        Ok(())
    }
}

struct PseudoEscape<T>(T);

impl<T: AsRef<str>> std::fmt::Display for PseudoEscape<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let needs_escape = self
            .0
            .as_ref()
            .chars()
            .any(|c| matches!(c, '"' | '\'') || c.is_control() || c.is_whitespace());
        let s: Cow<'_, str> = if needs_escape {
            Cow::Owned(format!("\"{}\"", self.0.as_ref().escape_debug()))
        } else {
            Cow::Borrowed(self.0.as_ref())
        };
        s.fmt(f)
    }
}

trait OsStrExt2 {
    fn has_prefix(&self, s: impl AsRef<OsStr>) -> bool;
}

impl OsStrExt2 for OsStr {
    fn has_prefix(&self, s: impl AsRef<OsStr>) -> bool {
        self.as_encoded_bytes()
            .starts_with(s.as_ref().as_encoded_bytes())
    }
}

// TODO: use `std::io::Write::write_all_vectored` once stabilised.
pub fn write_all_vectored(
    mut writer: impl std::io::Write,
    mut bufs: &mut [IoSlice<'_>],
) -> std::io::Result<()> {
    // The initial `advance_slices` call skips over any empty slices at the start of `bufs`.
    IoSlice::advance_slices(&mut bufs, 0);
    while !bufs.is_empty() {
        match writer.write_vectored(bufs) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            Ok(n) => IoSlice::advance_slices(&mut bufs, n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
