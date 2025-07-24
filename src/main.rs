use std::{
    borrow::Cow,
    cell::LazyCell,
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::Read,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

use base64::prelude::*;
use clap::Parser;
use color_eyre::{
    Result, Section, SectionExt,
    eyre::{Context, eyre},
};
use fs_at::OpenOptions as OpenOptionsAt;
use ltx_write::Ltx;
use merde::MerdeError;
use sha2::{
    Sha256,
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

    fn write_err(self, error: color_eyre::Report) {
        if self.enabled {
            eprintln!("{error:?}")
        } else {
            eprintln!("{error}")
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

fn with_cache<R: 'static>(
    from_disk_flag: bool,
    verb: VerbOutput,
    f: impl for<'a> FnOnce(&mut Cache<'a>) -> Result<R>,
) -> Result<R> {
    const CACHE_PATH: &str = ".agdatex-hashes.json";

    enum FromDisk<S> {
        CleanCache,
        LoadCache(S),
        Failure(color_eyre::Report),
    }

    impl<S> FromDisk<S> {
        fn try_next<R, E: Into<color_eyre::Report>>(
            self,
            f: impl FnOnce(S) -> Result<Option<R>, E>,
        ) -> FromDisk<R> {
            match self {
                FromDisk::CleanCache => FromDisk::CleanCache,
                FromDisk::Failure(report) => FromDisk::Failure(report),
                FromDisk::LoadCache(s) => match f(s) {
                    Ok(None) => FromDisk::CleanCache,
                    Ok(Some(r)) => FromDisk::LoadCache(r),
                    Err(e) => FromDisk::Failure(e.into()),
                },
            }
        }
    }

    let mut cache_json = String::new();

    let from_disk = if from_disk_flag {
        FromDisk::LoadCache(())
    } else {
        FromDisk::CleanCache
    };

    let from_disk = from_disk
        .try_next(|_| {
            let open_res = File::open(CACHE_PATH);
            if open_res
                .as_ref()
                .is_err_and(|e| matches!(e.kind(), std::io::ErrorKind::NotFound))
            {
                Ok(None)
            } else {
                open_res.map(Some)
            }
        })
        .try_next(|mut f| {
            f.read_to_string(&mut cache_json)
                .wrap_err("parsing cache data failed")
                .map(Some)
        })
        .try_next(|_| {
            merde_json::from_str::<Cache>(&cache_json)
                .map_err(|err| eyre!("{err}").wrap_err("parsing cache data failed"))
                .map(Some)
        });

    let mut cache = match from_disk {
        FromDisk::CleanCache => {
            if from_disk_flag {
                verb!(verb, "cache: {CACHE_PATH} does not exist");
            } else {
                verb!(verb, "cache: ignoring any cached state");
            }
            Cache::default()
        }
        FromDisk::LoadCache(c) => c,
        FromDisk::Failure(report) => {
            verb.write_err(report.wrap_err(format!("cache: loading {CACHE_PATH} failed")));
            Cache::default()
        }
    };

    let result = f(&mut cache)?;

    // If the main operation succeeded, try to write the cache back to the file system.
    let writeback = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(CACHE_PATH)
        .map_err(color_eyre::Report::from)
        .and_then(|mut file| {
            merde_json::to_writer(&mut file, &cache).map_err(color_eyre::Report::from)
        });
    if let Err(err) = writeback {
        verb.write_err(err.wrap_err("cache: serializing at {CACHE_PATH} failed"));
    }

    // Return the result.
    Ok(result)
}

fn main() -> Result<()> {
    color_eyre::install().unwrap();

    let args = Args::parse();
    let verb = VerbOutput {
        enabled: args.verbose,
    };

    if args.sources.is_empty() {
        return Err(eyre!("no inputs given"));
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

    let result = with_cache(!args.clear, verb, |cache| resolved_args.run(cache, sources));

    // Make sure the temporary directory is dropped at the very end.
    std::mem::drop(tmp_dir);
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct CachePath<P>(P);

impl<P: std::fmt::Display> std::fmt::Display for CachePath<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<P: AsRef<Path>> merde::Serialize for CachePath<P> {
    async fn serialize(
        &self,
        serializer: &mut dyn merde::DynSerializer,
    ) -> Result<(), MerdeError<'static>> {
        if let Some(str) = self.0.as_ref().to_str() {
            serializer.write(merde::Event::Str(str.into())).await
        } else {
            let start = merde::ArrayStart { size_hint: Some(1) };
            let encoded = BASE64_STANDARD_NO_PAD.encode(self.0.as_ref().as_os_str().as_bytes());
            serializer.write(merde::Event::ArrayStart(start)).await?;
            serializer.write(merde::Event::Str(encoded.into())).await?;
            serializer.write(merde::Event::ArrayEnd).await
        }
    }
}

impl<'s> merde::Deserialize<'s> for CachePath<Cow<'s, Path>> {
    async fn deserialize(de: &mut dyn merde::DynDeserializer<'s>) -> Result<Self, MerdeError<'s>> {
        let ev = de.next().await?;
        let path = if let merde::Event::ArrayStart(_) = ev {
            let encoded = de.next().await?.into_str()?;
            de.next().await?.into_array_end()?;
            let bytes = BASE64_STANDARD_NO_PAD
                .decode(encoded.as_bytes())
                .map_err(|err| MerdeError::StringParsingError {
                    format: "base64",
                    source: encoded,
                    index: 0,
                    message: err.to_string(),
                })?;
            Cow::Owned(PathBuf::from(OsString::from_vec(bytes)))
        } else {
            match ev.into_str()? {
                merde::CowStr::Borrowed(s) => Cow::Borrowed(Path::new(s)),
                merde::CowStr::Owned(s) => Cow::Owned(PathBuf::from(s.into_string())),
            }
        };
        Ok(CachePath(path))
    }
}

impl<'s, P: ToOwned + ?Sized> merde::IntoStatic for CachePath<Cow<'s, P>>
where
    P::Owned: 'static,
{
    type Output = CachePath<P::Owned>;

    fn into_static(self) -> Self::Output {
        CachePath(self.0.into_owned())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sha256Digest(GenericArray<u8, <Sha256 as OutputSizeUser>::OutputSize>);

impl merde::Serialize for Sha256Digest {
    async fn serialize(
        &self,
        serializer: &mut dyn merde::DynSerializer,
    ) -> Result<(), MerdeError<'static>> {
        serializer
            .write(merde::Event::Str(
                BASE64_STANDARD_NO_PAD.encode(self.0).into(),
            ))
            .await
    }
}

impl<'s> merde::Deserialize<'s> for Sha256Digest {
    async fn deserialize(de: &mut dyn merde::DynDeserializer<'s>) -> Result<Self, MerdeError<'s>> {
        let digest_str = de.next().await?.into_str()?;
        let mut sha256_buf = GenericArray::default();
        let err = match BASE64_STANDARD_NO_PAD.decode_slice(digest_str.as_bytes(), &mut sha256_buf)
        {
            Ok(n) if n == sha256_buf.len() => return Ok(Sha256Digest(sha256_buf)),
            Ok(n) => format!(
                "digest to short: expected {} bytes, got {n}",
                sha256_buf.len()
            ),
            Err(err) => err.to_string(),
        };
        Err(MerdeError::StringParsingError {
            format: "base64/sha356",
            source: digest_str,
            index: 0,
            message: err.to_string(),
        })
    }
}

struct CacheEntry {
    digest: Sha256Digest,
    fully_typechecked: bool,
}

merde::derive! {
    impl (Serialize, Deserialize) for struct CacheEntry {
        digest,
        fully_typechecked
    }
}

type Cache<'a> = HashMap<CachePath<Cow<'a, Path>>, CacheEntry>;

struct Agdatex {
    output_dir: PathBuf,
    temp_dir: PathBuf,
    verb: VerbOutput,
    state: State,
}

#[derive(Default)]
struct State {
    source_buffer: String,
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

    fn adjust_target_file_name(self, source: Cow<'_, Path>) -> Cow<'_, Path> {
        match self {
            FileAction::Copy => source,
            FileAction::Translate => {
                Cow::Owned(adjusted_translation_target_file_name(source.into_owned()))
            }
        }
    }
}

fn adjusted_translation_target_file_name(mut path: PathBuf) -> PathBuf {
    path.set_extension("lagda.tex");
    path
}

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
                    // Have to go this way until ErrorKind::FilesystemLoop is stabilized.
                    Err(err) if err.raw_os_error() == Some(libc::ELOOP) => None,
                    Err(err) => return Err(err.into()),
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
        Ok(match self {
            Item::UserInput { path } => {
                let target_path = adjusted_translation_target_file_name(base.as_ref().join(path));
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
                let target_name = adjusted_translation_target_file_name(name.into());
                OpenOptionsAt::default()
                    .write(fs_at::OpenOptionsWriteMode::Write)
                    .create(true)
                    .truncate(true)
                    .open_at(parent_fd_target, target_name)?
            }
        })
    }

    fn push_path_cap(&self, path_buf: &mut PathBuf, additional_cap: usize) {
        match self {
            Item::UserInput { path } => {
                path_buf.reserve(path.as_os_str().len() + 1 + additional_cap);
                path_buf.push(path)
            }
            Item::ChildItem {
                name, parent_item, ..
            } => {
                parent_item.push_path_cap(path_buf, additional_cap + 1 + name.len());
                path_buf.push(name);
            }
        }
    }

    fn push_path(&self, path_buf: &mut PathBuf) {
        self.push_path_cap(path_buf, 0);
    }

    fn to_path_buf(self) -> PathBuf {
        let mut buf = PathBuf::new();
        self.push_path(&mut buf);
        buf
    }
}

impl Agdatex {
    fn run(mut self, cache: &mut Cache, sources: Vec<PathBuf>) -> Result<()> {
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
                .arg("--only-scope-checking")
                .arg("--latex")
                .arg(&latex_dir_arg)
                .arg(translated)
                .expect_run()?;
        }

        Ok(())
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
        let item_target_path = action.adjust_target_file_name((&item_path).into());
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

                // Translate source.
                let needs_compile = self.translate_src(&item_path, out_file)?;
                if needs_compile {
                    self.state
                        .translated_item_paths
                        .push(item_target_path.into_owned());
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

    fn translate_src(&mut self, item_path: &Path, out_file: File) -> Result<bool> {
        let pp_path = LazyCell::new(|| item_path.display().to_string());
        let mut has_macro = false;

        // Run the translator.
        self.state.translator.run(
            &self.state.source_buffer,
            out_file,
            |diag| {
                self.state.diagnostics_count += 1;
                diag.to_report()
                    .print(NamedSource::new(&self.state.source_buffer, || {
                        LazyCell::force(&pp_path).clone()
                    }))
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
            Ltx::new()
                .command("input")
                .group(&item_path.as_os_str().to_string_lossy())
                .ln()
                .ln()
                .write(file)?;
        }

        // The file has to be compiled to LaTeX if it contained any macros.
        Ok(has_macro)
    }
}

struct NamedSource<'a, N> {
    get_name: N,
    source: ariadne::Source<&'a str>,
}

impl<'a, N> NamedSource<'a, N> {
    fn new(source: &'a str, get_name: N) -> Self {
        NamedSource {
            get_name,
            source: source.into(),
        }
    }
}

impl<'a, N: Fn() -> String> ariadne::Cache<()> for NamedSource<'a, N> {
    type Storage = &'a str;

    fn fetch(&mut self, id: &()) -> Result<&ariadne::Source<Self::Storage>, impl std::fmt::Debug> {
        self.source.fetch(id)
    }

    fn display<'x>(&self, _id: &'x ()) -> Option<impl std::fmt::Display + 'x> {
        Some((self.get_name)())
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
