use std::io::Write;

use ariadne::ColorGenerator;
use color_eyre::Result;

use crate::ltx_write::{Group, Ltx};
use crate::span_str::{Offset, Span, SpanStr};
use crate::string_stack::StringStack;

pub struct Macro<'a> {
    pub name: &'a str,
    pub line: u32,
    pub inline: bool,
}

#[derive(Default)]
pub struct Translator {
    namespaces: StringStack,
    hide_stack: HideStack,
}

type HideStack = Vec<Span>;

impl Translator {
    pub fn run(
        &mut self,
        src: &str,
        writer: impl Write,
        diag_fn: impl FnMut(Diagnostic) -> std::io::Result<()>,
        macro_fn: impl FnMut(Macro) -> std::io::Result<()>,
    ) -> Result<()> {
        let mut translation = Translation::new(
            writer,
            diag_fn,
            macro_fn,
            &mut self.namespaces,
            &mut self.hide_stack,
        );

        let src = SpanStr::new(src);
        for (line_no, line) in src.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                translation.add_empty_line(line.span().start)?;
            }

            match Command::try_parse(trimmed) {
                CommandParseResult::NotACommand => {
                    // Preserve line verbatim.
                    translation.add_verbatim(line)?;
                    continue;
                }
                CommandParseResult::InvalidCommand => {
                    // Diagnose the bad command and emit verbatim.
                    translation.diagnose(Diagnostic::InvalidCommand {
                        command_span: trimmed.span(),
                    })?;
                    translation.add_verbatim(line)?;
                }
                CommandParseResult::Command {
                    command,
                    trailing_chars,
                    invalid_inline_annot,
                } => {
                    if let Some(trailing_chars) = trailing_chars {
                        translation.diagnose(Diagnostic::CommandTrailingChars {
                            command_span: command.span().clone(),
                            trail_span: trailing_chars.span(),
                        })?;
                    }
                    if let Some(inline_annot) = invalid_inline_annot {
                        translation.diagnose(Diagnostic::CommandInvalidInline {
                            command_span: command.span().clone(),
                            inline_span: inline_annot.span(),
                        })?;
                    }
                    translation.add_command(
                        command,
                        line_no.try_into().expect("too many lines (> u32::MAX)"),
                    )?;
                }
            }
        }

        Ok(())
    }
}

struct Translation<'a, W, D, M> {
    output: W,
    diag_fn: D,
    macro_fn: M,
    mode: Mode,
    namespaces: &'a mut StringStack,
    hide_stack: &'a mut HideStack,
    prev_autoclose_macro: Option<Span>,
}

impl<'a, W, D, M> Translation<'a, W, D, M> {
    fn new(
        output: W,
        diag_fn: D,
        macro_fn: M,
        namespaces: &'a mut StringStack,
        hide_stack: &'a mut Vec<Span>,
    ) -> Self {
        namespaces.clear();
        hide_stack.clear();
        Translation {
            output,
            diag_fn,
            macro_fn,
            namespaces,
            hide_stack,
            mode: <_>::default(),
            prev_autoclose_macro: <_>::default(),
        }
    }
}

impl<W, D, M> Translation<'_, W, D, M>
where
    W: Write,
    D: FnMut(Diagnostic) -> std::io::Result<()>,
    M: FnMut(Macro) -> std::io::Result<()>,
{
    fn diagnose(&mut self, diagnostic: Diagnostic) -> Result<()> {
        (self.diag_fn)(diagnostic)?;
        Ok(())
    }

    fn add_empty_line(&mut self, offset: Offset) -> Result<()> {
        if matches!(
            self.mode,
            Mode::Macro(MacroMode {
                auto_close: true,
                ..
            })
        ) {
            self.close_macro(false, offset..offset + 1)?;
        }

        writeln!(self.output)?;
        Ok(())
    }

    fn add_verbatim(&mut self, line: SpanStr) -> Result<()> {
        match self.mode {
            Mode::None => {
                self.mode = Mode::Hide;
                Ltx::new()
                    .begin("code", "hide")
                    .ln()
                    .write(&mut self.output)?;
            }
            Mode::Macro(ref mut macro_mode) => {
                macro_mode.body.get_or_insert(line.span()).end = line.span().end;
            }
            _ => {}
        }

        writeln!(self.output, "{line}")?;
        Ok(())
    }

    fn add_command(&mut self, command: Command, line: u32) -> Result<()> {
        match command {
            Command::MacroStart {
                name,
                auto_close,
                inline,
                span,
            } => self.start_macro(
                name.as_str(),
                line,
                MacroMode {
                    start: span,
                    body: None,
                    inline,
                    auto_close,
                },
            )?,
            Command::MacroEnd(span) => {
                self.close_macro(true, span)?;
            }
            Command::NamespaceOpen { name, .. } => {
                self.namespaces.push(name.as_str());
            }
            Command::NamespaceClose(span) => {
                if self.namespaces.try_pop().is_none() {
                    self.diagnose(Diagnostic::NoNamespaceToClose { close_span: span })?;
                }
            }

            Command::HideStart(span) => {
                self.hide_stack.push(span);
            }
            Command::HideEnd(span) => {
                if self.hide_stack.pop().is_none() {
                    self.diagnose(Diagnostic::NoHideToClose { close_span: span })?;
                }
            }
        }

        Ok(())
    }

    fn start_macro(&mut self, name: &str, line: u32, macro_mode: MacroMode) -> Result<()> {
        // Ensure we aren't currently inside another macro and end any currently open
        // `\begin{code}[hide]` environments.
        self.transition_to_none_mode(macro_mode.start.clone())?;

        struct NameGuard<'a>(&'a mut StringStack);

        impl<'a> NameGuard<'a> {
            fn new(stack: &'a mut StringStack, name: &str) -> Self {
                stack.push(name);
                NameGuard(stack)
            }
        }

        impl std::ops::Deref for NameGuard<'_> {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                self.0.as_str()
            }
        }

        impl std::fmt::Display for NameGuard<'_> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self)
            }
        }

        impl Drop for NameGuard<'_> {
            fn drop(&mut self) {
                self.0.pop();
            }
        }

        // Temporarily push the name to the name stack so that we have the full name available as a string
        // slice.
        let name = NameGuard::new(self.namespaces, name);
        (self.macro_fn)(Macro {
            name: &name,
            line,
            inline: macro_mode.inline,
        })?;

        // Non-inline macros come in a starred and unstarred form. Inline macros do not take any
        // arguments.
        let macro_spec = if macro_mode.inline { "" } else { "s" };
        let ltx = Ltx::new()
            .with_comment(|ltx| ltx.command(&name))
            .command("NewDocumentCommand")
            .command(&name)
            .group(macro_spec)
            .push(Group::TEX.open)
            .pctln();

        if macro_mode.inline {
            ltx.begin("code", "inline").ln().write(&mut self.output)?;
        } else {
            ltx.command("IfBooleanF")
                .group("#1")
                .with_group(Group::TEX, |ltx| {
                    ltx.begin_("AgdaSuppressSpace").begin_("AgdaAlign")
                })
                .pctln()
                .begin_("code")
                .ln()
                .write(&mut self.output)?;
        }

        // Finally, go into macro recording mode.
        self.mode = Mode::Macro(macro_mode);
        Ok(())
    }

    fn transition_to_none_mode(&mut self, span: Span) -> Result<()> {
        match std::mem::replace(&mut self.mode, Mode::None) {
            // Nothing to do.
            Mode::None => {}

            // End the current \begin{code}[hide] segment.
            Mode::Hide => {
                Ltx::new().end("code").ln().ln().write(&mut self.output)?;
            }

            // Diagnose the open macro and close it.
            Mode::Macro(macro_mode) => {
                self.diagnose(Diagnostic::OpenMacro {
                    cur_start_span: macro_mode.start.clone(),
                    cur_macro_span: macro_mode.body.clone(),
                    new_start_span: span,
                })?;
                self.close_macro_mode(macro_mode, CloseMode::Forced)?;
            }
        }

        Ok(())
    }

    fn close_macro(&mut self, explicit: bool, span: Span) -> Result<()> {
        match std::mem::take(&mut self.mode) {
            Mode::Macro(macro_) => {
                let close_mode = if explicit {
                    CloseMode::Explicit(span)
                } else {
                    CloseMode::Auto(span)
                };
                self.close_macro_mode(macro_, close_mode)?;
            }

            // If there is no macro we could close we'll emit a diagnostic and ignore the command.
            Mode::None | Mode::Hide => {
                self.diagnose(Diagnostic::NoMacroToClose {
                    end_span: span,
                    prev_autoclose_macro: self.prev_autoclose_macro.clone(),
                })?;
            }
        }
        Ok(())
    }

    fn close_macro_mode(&mut self, macro_mode: MacroMode, close_mode: CloseMode) -> Result<()> {
        // If we are explicitly closing an auto-close macro emit a diganostic and ignore!
        // Because we ignore here, we don't touch the hide-stack at all.
        if let CloseMode::Explicit(ref span) = close_mode
            && macro_mode.auto_close
        {
            self.diagnose(Diagnostic::MacroCloseExplicit {
                macro_start: macro_mode.start.clone(),
                macro_end: span.clone(),
            })?;
            self.mode = Mode::Macro(macro_mode);
            return Ok(());
        }

        // If the hide-stack is non-empty at this point emit an error but swallow all the
        // open hides.
        if let Some(last_unclosed) = self.hide_stack.last() {
            match close_mode {
                CloseMode::Explicit(ref span) | CloseMode::Auto(ref span) => {
                    let dia = Diagnostic::MacroCloseHideStack {
                        macro_start: macro_mode.start.clone(),
                        macro_end: span.clone(),
                        open_hide: last_unclosed.clone(),
                    };
                    self.diagnose(dia)?;
                }
                CloseMode::Forced => {
                    // Swallow all unclosed hides.
                }
            }
            self.hide_stack.clear();
        }

        // If we are finishing an auto-close macro keep track of this for error messages.
        self.prev_autoclose_macro = match close_mode {
            CloseMode::Auto(span) | CloseMode::Explicit(span) if macro_mode.auto_close => {
                Some(macro_mode.start.start..span.end)
            }
            _ => None,
        };

        // Write the closing LaTeX code. Inline macros get
        //
        // ```latex
        // \end{code}}
        // ```
        //
        // whilst non-inline macros get
        //
        // ```latex
        // \end{code}%
        // \IfBooleanF{#1}{\end{AgdaSuppressSpace}\end{AgdaAlign}}}"
        // ```
        if macro_mode.inline {
            Ltx::new()
                .end("code")
                .push(Group::TEX.close)
                .ln()
                .ln()
                .write(&mut self.output)?;
        } else {
            Ltx::new()
                .end("code")
                .pctln()
                .command("IfBooleanF")
                .group("#1")
                .with_group(Group::TEX, |ltx| {
                    ltx.end("AgdaSuppressSpace").end("AgdaAlign")
                })
                .push(Group::TEX.close)
                .ln()
                .ln()
                .write(&mut self.output)?;
        };
        Ok(())
    }
}

enum CloseMode {
    Auto(Span),
    Explicit(Span),
    Forced,
}

#[derive(Debug, Clone)]
pub enum Diagnostic {
    OpenMacro {
        cur_start_span: Span,
        cur_macro_span: Option<Span>,
        new_start_span: Span,
    },
    MacroCloseExplicit {
        macro_start: Span,
        macro_end: Span,
    },
    MacroCloseHideStack {
        macro_start: Span,
        macro_end: Span,
        open_hide: Span,
    },
    NoMacroToClose {
        end_span: Span,
        prev_autoclose_macro: Option<Span>,
    },
    NoNamespaceToClose {
        close_span: Span,
    },
    NoHideToClose {
        close_span: Span,
    },
    InvalidCommand {
        command_span: Span,
    },
    CommandTrailingChars {
        command_span: Span,
        trail_span: Span,
    },
    CommandInvalidInline {
        command_span: Span,
        inline_span: Span,
    },
}

impl Diagnostic {
    pub fn to_report<'a>(&self) -> ariadne::Report<'a, Span> {
        let mut colors = ColorGenerator::new();
        let build = |span: &Span, msg: &str| {
            ariadne::Report::build(ariadne::ReportKind::Error, span.clone()).with_message(msg)
        };

        macro_rules! label {
            ($span:expr, $($arg:tt)*) => {
                ::ariadne::Label::new(Span::clone(&$span))
                    .with_message(format!($($arg)*))
                    .with_color(colors.next())
            };
        }

        match self {
            Diagnostic::OpenMacro {
                cur_start_span,
                cur_macro_span,
                new_start_span,
            } => build(
                new_start_span,
                "starting new macro without closing previous macro",
            )
            .with_label(label!(new_start_span, "new macro started here"))
            .with_label(label!(cur_start_span, "previous macro started here"))
            .with_labels(
                cur_macro_span
                    .as_ref()
                    .map(|span| label!(span, "previous macro body").with_priority(-1)),
            ),
            Diagnostic::MacroCloseExplicit {
                macro_start,
                macro_end,
            } => build(macro_end, "unexpected macro closure")
                .with_label(label!(macro_start, "auto-closing macro started here")),
            Diagnostic::MacroCloseHideStack {
                macro_start,
                macro_end,
                open_hide,
            } => build(macro_end, "closing macro while hide-block is open")
                .with_label(label!(
                    macro_start.start..macro_end.end,
                    "macro started here",
                ))
                .with_label(label!(
                    open_hide,
                    "the hidden section started here is incomplete",
                )),
            Diagnostic::NoMacroToClose {
                end_span,
                prev_autoclose_macro,
            } => build(end_span, "unexpected macro closure")
                .with_label(label!(end_span, "there is no macro to close at this point"))
                .with_labels(
                    prev_autoclose_macro
                        .as_ref()
                        .map(|span| label!(span, "previous auto-closing macro")),
                ),
            Diagnostic::NoNamespaceToClose { close_span } => {
                build(close_span, "unexpected namespace closure").with_label(label!(
                    close_span,
                    "there is no open namespace to close at this point",
                ))
            }
            Diagnostic::NoHideToClose { close_span } => {
                build(close_span, "unexpected hide-segment closure").with_label(label!(
                    close_span,
                    "there is no open hide-segment at this point"
                ))
            }
            Diagnostic::InvalidCommand { command_span } => {
                build(command_span, "invalid agdatex command")
                    .with_label(label!(command_span, "I do not understand this command"))
            }
            Diagnostic::CommandTrailingChars {
                command_span,
                trail_span,
            } => build(trail_span, "trailing characters in command")
                .with_label(label!(
                    command_span,
                    "I can parse this as a valid command..."
                ))
                .with_label(label!(
                    trail_span,
                    "...but I do not know what to do with these characters"
                )),
            Diagnostic::CommandInvalidInline {
                command_span,
                inline_span,
            } => build(inline_span, "invalid inline marker in command")
                .with_label(label!(inline_span, "this inline marker is invalid"))
                .with_label(label!(command_span, "in this non-macro start command")),
        }
        .finish()
    }
}

/// While processing a file line by line we have to keep track in which context we are currently
/// reading the file.
#[derive(Debug, Default, Clone)]
enum Mode {
    #[default]
    None,
    Hide,
    Macro(MacroMode),
}

#[derive(Debug, Default, Clone)]
struct MacroMode {
    start: Span,
    body: Option<Span>,
    inline: bool,
    auto_close: bool,
}

#[derive(Debug, Clone)]
enum Command<'a> {
    MacroStart {
        name: SpanStr<'a>,
        auto_close: bool,
        inline: bool,
        span: Span,
    },
    MacroEnd(Span),
    NamespaceOpen {
        name: SpanStr<'a>,
        span: Span,
    },
    NamespaceClose(Span),
    HideStart(Span),
    HideEnd(Span),
}

enum CommandParseResult<'a> {
    NotACommand,
    InvalidCommand,
    Command {
        command: Command<'a>,
        trailing_chars: Option<SpanStr<'a>>,
        invalid_inline_annot: Option<SpanStr<'a>>,
    },
}

impl<'a> Command<'a> {
    fn span(&self) -> &Span {
        match self {
            Command::MacroStart { span, .. } => span,
            Command::MacroEnd(span) => span,
            Command::NamespaceOpen { span, .. } => span,
            Command::NamespaceClose(span) => span,
            Command::HideStart(span) => span,
            Command::HideEnd(span) => span,
        }
    }

    fn try_parse(str: SpanStr<'a>) -> CommandParseResult<'a> {
        let whole_span = str.span();
        let Some(s) = str.strip_prefix("--!") else {
            return CommandParseResult::NotACommand;
        };

        let (s, inline) = if let Some(s) = s.strip_prefix("!") {
            (s, true)
        } else {
            (s, false)
        };

        let (name, rest) = s
            .trim_start()
            .split_once(|c: char| !c.is_ascii_alphabetic())
            .map(|(name, rest)| (name, rest.trim_start()))
            .unwrap_or((s, SpanStr::with_offset("", s.span().end)));

        let mut rest = rest.as_str().chars();
        let next_char = rest.next();
        let command_span = whole_span.start..whole_span.end - rest.as_str().len();

        let command = match next_char {
            None if !name.is_empty() => Command::MacroStart {
                name,
                inline,
                auto_close: true,
                span: command_span,
            },

            Some('{') if !name.is_empty() => Command::MacroStart {
                name,
                inline,
                auto_close: false,
                span: command_span,
            },

            Some('}') if name.is_empty() => Command::MacroEnd(command_span),

            Some('>') if !name.is_empty() => Command::NamespaceOpen {
                name,
                span: command_span,
            },
            Some('<') if name.is_empty() => Command::NamespaceClose(command_span),

            Some('[') if name.is_empty() => Command::HideStart(command_span),
            Some(']') if name.is_empty() => Command::HideEnd(command_span),

            _ => return CommandParseResult::InvalidCommand,
        };

        let remainder = rest.as_str();
        let trailing_chars = (!remainder.is_empty()).then_some(SpanStr::with_offset(
            remainder,
            whole_span.end - remainder.len(),
        ));
        let has_bad_inline = inline && !matches!(command, Command::MacroStart { .. });
        let invalid_inline_annot = has_bad_inline.then_some(str.get(..4));
        CommandParseResult::Command {
            command,
            trailing_chars,
            invalid_inline_annot,
        }
    }
}

/*
trait WriteLateX: std::io::Write {
    fn ltx_cmd(&mut self, cmd: &str) -> std::io::Result<&mut Self> {
        self.write_fmt(format_args!(r"\{cmd}"))?;
        Ok(self)
    }

    fn ltx_begin(&mut self, env: &str) -> std::io::Result<&mut Self> {
        self.ltx_cmd("begin")?.ltx_group(group!(env))
    }

    fn ltx_end(&mut self, env: &str) -> std::io::Result<&mut Self> {
        self.ltx_cmd("end")?.ltx_group(group!(env))
    }

    fn ltx_group<T: AsRef<str>>(&mut self, inner: Group<T>) -> std::io::Result<&mut Self> {
        self.with_ltx_group(
            inner.map(|x| move |out: &mut Self| out.write_all(x.as_ref().as_bytes())),
        )?;
        Ok(self)
    }

    fn with_ltx_group<R>(
        &mut self,
        group: Group<impl FnOnce(&mut Self) -> std::io::Result<R>>,
    ) -> std::io::Result<R> {
        let (open, f, close) = group.into_parts();
        self.write_all(&[open])?;
        let r = f(self)?;
        self.write_all(&[close])?;
        Ok(r)
    }

    fn ltx_nl(&mut self, percent: bool) -> std::io::Result<()> {
        self.write_all(if percent { b"\n%" } else { b"\n" })
    }
}

impl<W: std::io::Write> WriteLateX for W {}
*/
