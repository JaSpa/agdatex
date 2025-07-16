use std::{
    borrow::Cow,
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::Read,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::Command,
};

use base64::prelude::*;
use clap::Parser;
use color_eyre::{
    Result, Section, SectionExt,
    eyre::{Context, eyre},
};
use merde::MerdeError;
use sha2::{
    Sha256,
    digest::{OutputSizeUser, generic_array::GenericArray},
};
use tempdir::TempDir;

//mod contiguous_stack;
//use contiguous_stack::PathStack;

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

    fn writeln<Msg: std::fmt::Display>(self, msg: impl FnOnce() -> Msg) {
        self.if_enabled(|| eprintln!("{}", msg()));
    }

    fn write_err(self, error: color_eyre::Report) {
        if self.enabled {
            eprintln!("{error:?}")
        } else {
            eprintln!("{error}")
        }
    }
}

#[derive(Debug, Parser)]
struct Args {
    /// Output directory for Agda's LaTeX backend; forwarded as `--latex-dir`.
    #[clap(short, long = "outputdir", default_value = "latex")]
    output_dir: PathBuf,

    /// This file will `\input` all generated .tex files. Both .tex and .sty are supported.
    /// [default: `<OUTPUTDIR>/agda-generated.sty`]
    #[clap(short, long = "exportfile")]
    export_file: Option<PathBuf>,

    /// Write full path to the generated .tex files into <EXPORTFILE>.
    #[clap(short, long = "fullpath")]
    full_path: bool,

    /// Temporary directory to copy the project root to. (default: fresh system-dependent temporary
    /// directory.
    #[clap(short, long = "tempdir")]
    temp_dir: Option<PathBuf>,

    /// Project root. [default: `git rev-parse --show-toplevel`]
    #[clap(short, long)]
    root: Option<PathBuf>,

    /// Write the list of generated macros to this file.
    #[clap(short, long)]
    index: Option<PathBuf>,

    /// Enable verbose output.
    #[clap(short, long)]
    verbose: bool,

    /// Clear caches to force a rebuild of all modules.
    #[clap(short, long)]
    clear: bool,

    /// Paths to annotated .agda files.
    sources: Vec<PathBuf>,
}

fn discover_root() -> Result<PathBuf> {
    let mut output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .success_output()?;

    // Trim a trailing newline from the output.
    if output.stdout.last() == Some(&b'\n') {
        _ = output.stdout.pop();
    }

    // Turn output into a PathBuf.
    let output_str = OsString::from_vec(output.stdout);
    let root_path = PathBuf::from(output_str);
    Ok(root_path)
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
                verb.writeln(|| format!("cache: {CACHE_PATH} does not exist"));
            } else {
                verb.writeln(|| "cache: ignoring any cached state");
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

    verb.writeln(|| format!("Command line options: {args:#?}"));

    let root = if let Some(root) = args.root {
        root.canonicalize()?
    } else {
        discover_root().wrap_err("failed to detect project root; try `--root <ROOT>`")?
    };
    verb.writeln(|| format!("Resolved project root: {}", root.display()));

    let sources = args
        .sources
        .into_iter()
        .map(|src| normalize_source(src, &root))
        .collect::<Result<Vec<_>>>()?;
    verb.writeln(|| format!("Canonicalized sources: {sources:#?}"));

    let mut tmp_dir: Option<TempDir> = None;
    let resolved_args = Agdatex {
        verb,
        root,
        output_dir: args.output_dir,
        export_file: args.export_file,
        index: args.index,
        full_path: args.full_path,
        temp_dir: if let Some(tmp) = args.temp_dir {
            tmp
        } else {
            let tmp = TempDir::new("agdatex")?;
            let path = tmp.path().to_path_buf();
            tmp_dir = Some(tmp);
            path
        },
        source_buffer: String::new(),
        translated_paths: Vec::new(),
    };

    verb.writeln(|| format!("Temporary directory: {}", resolved_args.temp_dir.display(),));

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
    root: PathBuf,
    output_dir: PathBuf,
    export_file: Option<PathBuf>,
    index: Option<PathBuf>,
    full_path: bool,
    temp_dir: PathBuf,
    verb: VerbOutput,
    source_buffer: String,
    translated_paths: Vec<PathBuf>,
}

enum Item<'a, 'b> {
    UserInput {
        path: PathBuf,
    },
    ChildItem {
        parent_fd: &'a File,
        parent_path: &'a mut ChildPathBuf<'b>,
        name: &'a OsStr,
    },
}

#[derive(Debug, Clone, Copy)]
enum FileAction {
    Translate,
    Copy,
}

impl Item<'_, '_> {
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

    fn open(&self) -> Result<Option<File>> {
        Ok(match self {
            Item::UserInput { path } => Some(File::open(path)?),
            Item::ChildItem {
                parent_fd, name, ..
            } => {
                let res = fs_at::OpenOptions::default()
                    .read(true)
                    .follow(false)
                    .open_at(parent_fd, name);
                match res {
                    Ok(fd) => Some(fd),
                    // Have to go this way until ErrorKind::FilesystemLoop is stabilized.
                    Err(err) if err.raw_os_error() == Some(libc::ELOOP) => None,
                    Err(err) => return Err(err.into()),
                }
            }
        })
    }

    fn derive_child_path_buf(&mut self) -> ChildPathBuf<'_> {
        match self {
            Item::UserInput { path } => ChildPathBuf::RootPath(path),
            Item::ChildItem {
                parent_path, name, ..
            } => parent_path.push_child(name),
        }
    }
}

#[derive(Debug)]
enum ChildPathBuf<'a> {
    RootPath(&'a mut PathBuf),
    ChildPath(&'a mut PathBuf),
}

impl ChildPathBuf<'_> {
    fn get_buf_mut(&mut self) -> &mut PathBuf {
        match self {
            ChildPathBuf::RootPath(buf) => buf,
            ChildPathBuf::ChildPath(buf) => buf,
        }
    }

    fn push_child(&mut self, child: impl AsRef<OsStr>) -> ChildPathBuf<'_> {
        self.get_buf_mut().push(child.as_ref());
        ChildPathBuf::ChildPath(self.get_buf_mut())
    }
}

impl AsRef<Path> for ChildPathBuf<'_> {
    fn as_ref(&self) -> &Path {
        match self {
            ChildPathBuf::RootPath(buf) => buf,
            ChildPathBuf::ChildPath(buf) => buf,
        }
    }
}

impl std::ops::Deref for ChildPathBuf<'_> {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

impl Drop for ChildPathBuf<'_> {
    fn drop(&mut self) {
        if let ChildPathBuf::ChildPath(buf) = self {
            let popped = buf.pop();
            assert!(popped, "ChildPathBuf misused")
        }
    }
}

impl Agdatex {
    fn run(mut self, cache: &mut Cache, sources: Vec<PathBuf>) -> Result<()> {
        for input_path in sources {
            self.translate_item(Item::UserInput { path: input_path })?;
        }

        Ok(())
    }

    fn translate_item(&mut self, item: Item) -> Result<()> {
        let Some(fd_item) = item.open()? else {
            return Ok(());
        };

        if fd_item.metadata()?.file_type().is_dir() {
            self.translate_dir(item, fd_item)?;
        } else {
            self.translate_file(item, fd_item)?;
        }

        Ok(())
    }

    fn translate_dir(&mut self, mut parent_item: Item, fd: File) -> Result<()> {
        let mut path_buf = parent_item.derive_child_path_buf();

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
                parent_fd: &fd,
                parent_path: &mut path_buf,
                name,
            })?;
        }

        Ok(())
    }

    fn translate_file(&mut self, mut item: Item, mut file: File) -> Result<()> {
        // Determine what to do with this file based on the extension.
        //
        // Skip if neither a .agda or .lagda/.lagda.* file.
        let Some(action) = item.file_action()? else {
            self.verb
                .writeln(|| format!("SKIP {}", item.derive_child_path_buf().display()));
            return Ok(());
        };

        let item_path = item.derive_child_path_buf();
        let target_path = self.temp_dir.join(&item_path);
        self.verb.writeln(|| {
            format!(
                "{action:?} {} to {}",
                item_path.display(),
                target_path.display(),
            )
        });

        match action {
            // A .agda file. Translate annotations into LaTeX macro definitions.
            FileAction::Translate => {
                self.source_buffer.clear();
                file.read_to_string(&mut self.source_buffer)?;
                self.translate_src(&item_path, &target_path)?;
                Ok(())
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
            //   our `tempdir` is on another device.
            //
            // For now, we ignore FD-relativity and use `std::fs::copy.
            FileAction::Copy => {
                std::fs::copy(&item_path, target_path)?;
                Ok(())
            }
        }
    }

    fn translate_src(&mut self, path: &Path, target: &Path) -> Result<()> {
        todo!()
    }
}

pub trait CommandSuccess {
    fn success_output(&mut self) -> Result<std::process::Output>;
}

impl CommandSuccess for Command {
    fn success_output(&mut self) -> Result<std::process::Output> {
        let cmd_section = |cmd: &Command| format!("{cmd:?}").header("Command");
        let with_out_section = |report: color_eyre::Report, header: &'static str, out: &[u8]| {
            if out.is_empty() {
                report
            } else {
                report.section(String::from_utf8_lossy(out).into_owned().header(header))
            }
        };

        let out = self
            .output()
            .wrap_err("process invocation failed")
            .with_section(|| cmd_section(self))?;

        if out.status.success() {
            return Ok(out);
        }

        let mut report = eyre!(
            "command {} failed with exit code {}",
            PseudoEscape(self.get_program().to_string_lossy()),
            out.status
        )
        .section(cmd_section(self));
        report = with_out_section(report, "Stdout", &out.stdout);
        report = with_out_section(report, "Stderr", &out.stderr);
        Err(report)
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

trait PathExt {
    fn is_hidden(&self) -> bool;
}

impl PathExt for Path {
    fn is_hidden(&self) -> bool {
        if let Some(name_str) = self.file_name().and_then(OsStr::to_str) {
            name_str.as_bytes().first() == Some(&b'.')
        } else {
            false
        }
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
