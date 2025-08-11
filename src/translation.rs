use std::io::Write;

use ariadne::ColorGenerator;
use color_eyre::Result;

use crate::ltx_write::{Group, Ltx, Nat};
use crate::span_str::{Span, SpanStr};
use crate::string_stack::StringStack;

#[derive(Debug, Clone, Copy)]
pub struct Macro<S> {
    pub name: S,
    pub line: u32,
    pub inline: bool,
}

impl<S> Macro<S> {
    pub fn to_ltx_comment(&self) -> Ltx<'_, impl Nat>
    where
        S: AsRef<str>,
    {
        self.push_ltx_comment(Ltx::new())
    }

    pub fn push_ltx_comment<'a>(&'a self, ltx: Ltx<'a, impl Nat>) -> Ltx<'a, impl Nat>
    where
        S: AsRef<str>,
    {
        ltx.with_comment(move |l| {
            l.command(self.name.as_ref())
                .push(if self.inline { " [inline]" } else { "" })
        })
    }

    pub fn to_owned(&self) -> Macro<String>
    where
        S: AsRef<str>,
    {
        Macro {
            name: self.name.as_ref().to_owned(),
            line: self.line,
            inline: self.inline,
        }
    }
}

impl<S: AsRef<str>> std::fmt::Display for Macro<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.to_ltx_comment().write_fmt(f)
    }
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
        mut writer: impl Write,
        diag_fn: impl FnMut(Diagnostic) -> std::io::Result<()>,
        macro_fn: impl FnMut(Macro<&str>) -> std::io::Result<()>,
    ) -> Result<()> {
        let mut translation = Translation::new(
            &mut writer,
            diag_fn,
            macro_fn,
            &mut self.namespaces,
            &mut self.hide_stack,
        );

        let src = SpanStr::new(src);
        for (line_no, line) in src.lines().enumerate() {
            let line_span = line.line_span();
            let line = line.as_span_str();
            let trimmed = line.trim();
            if trimmed.is_empty() {
                translation.add_empty_line(line_span)?;
                continue;
            }

            match Command::try_parse(trimmed) {
                CommandParseResult::NotACommand => {
                    // Preserve line verbatim.
                    translation.add_verbatim(line.as_str(), line_span, true)?;
                    continue;
                }
                CommandParseResult::InvalidCommand => {
                    // Diagnose the bad command and emit verbatim.
                    translation.diagnose(Diagnostic::InvalidCommand {
                        command_span: trimmed.span(),
                    })?;
                    translation.add_verbatim(line.as_str(), line_span, true)?;
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

        translation.transition_to_none_mode(src.span().end..src.span().end, true)?;
        Ok(())
    }
}

struct Translation<'a, D, M> {
    output: &'a mut dyn Write,
    diag_fn: D,
    macro_fn: M,
    mode: Mode,
    namespaces: &'a mut StringStack,
    hide_stack: &'a mut HideStack,
    prev_autoclose_macro: Option<Span>,
}

impl<'a, D, M> Translation<'a, D, M> {
    fn new(
        output: &'a mut dyn Write,
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

fn union_span(dst: &mut Option<Span>, span: Span) {
    match dst {
        Some(s) => {
            s.start = s.start.min(span.start);
            s.end = s.end.max(span.end);
        }
        None => *dst = Some(span),
    }
}

impl<D, M> Translation<'_, D, M>
where
    D: FnMut(Diagnostic) -> std::io::Result<()>,
    M: FnMut(Macro<&str>) -> std::io::Result<()>,
{
    fn diagnose(&mut self, diagnostic: Diagnostic) -> Result<()> {
        (self.diag_fn)(diagnostic)?;
        Ok(())
    }

    fn add_empty_line(&mut self, line_span: Span) -> Result<()> {
        if matches!(
            self.mode,
            Mode::Macro(MacroMode {
                auto_close: true,
                ..
            })
        ) {
            self.close_macro(false, line_span)?;
        } else {
            self.add_verbatim("", line_span, false)?;
        }
        Ok(())
    }

    fn add_verbatim(&mut self, line: &str, line_span: Span, need_hide: bool) -> Result<()> {
        match self.mode {
            Mode::None if need_hide => {
                self.mode = Mode::Hide;
                Ltx::new()
                    .begin("code", "hide")
                    .ln()
                    .write(&mut self.output)?;
            }
            Mode::Macro(ref mut macro_mode) => {
                union_span(&mut macro_mode.body, line_span);
            }
            _ => {}
        }

        self.ensure_correct_macro_mode()?;
        Ltx::new().push(line).ln().write(&mut self.output)?;

        Ok(())
    }

    fn ensure_correct_macro_mode(&mut self) -> Result<()> {
        let Mode::Macro(ref mut macro_mode) = self.mode else {
            return Ok(());
        };

        let hide = !self.hide_stack.is_empty();
        if hide && matches!(macro_mode.inner_mode, Mode::Hide)
            || !hide && matches!(macro_mode.inner_mode, Mode::Macro(_))
        {
            return Ok(());
        }

        fn push_code_begin<'a>(
            ltx: Ltx<'a, impl Nat>,
            code_arg: &'static str,
        ) -> Ltx<'a, impl Nat> {
            ltx.begin("code", code_arg).ln()
        }

        let code_arg = if hide {
            "hide"
        } else if macro_mode.inline {
            r"\IfBooleanTF{#1}{inline*}{inline}"
        } else {
            ""
        };
        match macro_mode.inner_mode {
            Mode::None => {
                push_code_begin(Ltx::new(), code_arg).write(&mut self.output)?;
            }
            Mode::Hide | Mode::Macro(()) => {
                push_code_begin(Ltx::new().end("code").pctln(), code_arg)
                    .write(&mut self.output)?;
            }
        }

        macro_mode.inner_mode = if hide { Mode::Hide } else { Mode::Macro(()) };
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
                    inner_mode: Mode::None,
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
        assert_eq!(
            macro_mode.inner_mode,
            Mode::None,
            "macros need to start in `Mode::None`"
        );

        // Ensure we aren't currently inside another macro and end any currently open
        // `\begin{code}[hide]` environments.
        self.transition_to_none_mode(macro_mode.start.clone(), false)?;

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
        let macro_: Macro<&str> = Macro {
            name: &name,
            line,
            inline: macro_mode.inline,
        };
        (self.macro_fn)(macro_)?;

        /*
         * \NewDocumentCommand\NAME{s}{
         */
        let ltx = macro_
            .to_ltx_comment()
            .command("NewDocumentCommand")
            .command(&name)
            .group("s")
            .push(Group::TEX.open)
            .pctln();

        if macro_mode.inline {
            ltx.write(&mut self.output)?;
        } else {
            ltx.command("IfBooleanF")
                .group("#1")
                .with_group(Group::TEX, |ltx| ltx.begin_("AgdaMultiCode"))
                .pctln()
                .write(&mut self.output)?;
        }

        // Finally, go into macro recording mode.
        self.mode = Mode::Macro(macro_mode);
        Ok(())
    }

    fn transition_to_none_mode(&mut self, span: Span, file_end: bool) -> Result<()> {
        match std::mem::replace(&mut self.mode, Mode::None) {
            // Nothing to do.
            Mode::None => {}

            // End the current \begin{code}[hide] segment.
            Mode::Hide => {
                Ltx::new().end("code").ln().ln().write(&mut self.output)?;
            }

            // Diagnose the open macro and close it.
            Mode::Macro(macro_mode) => {
                // Don't diagnose if it is a auto-close macro and the end of the file.
                let auto_at_end = macro_mode.auto_close && file_end;
                if !auto_at_end {
                    let cur_start_span = macro_mode.start.clone();
                    let cur_macro_span = macro_mode.body.clone();
                    let dia = if file_end {
                        Diagnostic::UnfinishedMacro {
                            cur_start_span,
                            cur_macro_span,
                            file_end_span: span,
                        }
                    } else {
                        Diagnostic::OpenMacro {
                            cur_start_span,
                            cur_macro_span,
                            new_start_span: span,
                        }
                    };
                    self.diagnose(dia)?;
                }

                // We're at the end of the file or forced the macro end.
                self.prev_autoclose_macro = None;
                // Swallow any unclosed hides.
                self.hide_stack.clear();

                // Emit the close code.
                self.emit_macro_close(macro_mode)?;
            }
        }

        Ok(())
    }

    fn close_macro(&mut self, explicit: bool, span: Span) -> Result<()> {
        match std::mem::take(&mut self.mode) {
            Mode::Macro(macro_) => {
                // If we are explicitly closing an auto-close macro we'll emit a diganostic.
                if explicit && macro_.auto_close {
                    self.diagnose(Diagnostic::MacroCloseExplicit {
                        macro_start: macro_.start.clone(),
                        macro_end: span.clone(),
                        macro_body: macro_.body.clone(),
                    })?;
                }

                // If the hide-stack is non-empty at this point we'll emit an error.
                if let Some(last_unclosed) = self.hide_stack.last() {
                    self.diagnose(Diagnostic::MacroCloseHideStack {
                        macro_start: macro_.start.clone(),
                        macro_end: span.clone(),
                        open_hide: last_unclosed.clone(),
                    })?;
                }

                // If we are finishing an auto-close macro keep track of this for error messages.
                self.prev_autoclose_macro = if macro_.auto_close {
                    Some(macro_.start.start..span.end)
                } else {
                    None
                };

                // Emit the latex code to close the macro.
                self.emit_macro_close(macro_)?;
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

    /// Writes the closing LaTeX code. If [`macro_mode.inner_mode`][MacroMode::inner_mode] is
    /// [`Mode::None`] no `\end{code}` will be emitted.
    ///
    /// Inline are terminated with
    ///
    /// ```latex
    /// \end{code}}
    /// ```
    ///
    /// Non-inline macros are terminated with
    ///
    /// ```latex
    /// \end{code}%
    /// \IfBooleanF{#1}{\end{AgdaSuppressSpace}\end{AgdaAlign}}}
    /// ```
    fn emit_macro_close(&mut self, macro_mode: MacroMode) -> Result<()> {
        fn push_macro_suffix(ltx: Ltx<'_, impl Nat>) -> Ltx<'_, impl Nat> {
            ltx.pctln()
                .command("IfBooleanF")
                .group("#1")
                .with_group(Group::TEX, |ltx| ltx.end("AgdaMultiCode"))
        }

        fn push_macro_close(ltx: Ltx<'_, impl Nat>) -> Ltx<'_, impl Nat> {
            ltx.push(Group::TEX.close).ln().ln()
        }

        let end_ltx = Ltx::new().end("code");
        match macro_mode.inner_mode {
            Mode::None => {
                push_macro_close(Ltx::new()).write(&mut self.output)?;
            }
            Mode::Hide | Mode::Macro(_) if macro_mode.inline => {
                push_macro_close(end_ltx).write(&mut self.output)?;
            }
            Mode::Hide | Mode::Macro(_) => {
                push_macro_close(push_macro_suffix(end_ltx)).write(&mut self.output)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum Diagnostic {
    OpenMacro {
        cur_start_span: Span,
        cur_macro_span: Option<Span>,
        new_start_span: Span,
    },
    UnfinishedMacro {
        cur_start_span: Span,
        cur_macro_span: Option<Span>,
        file_end_span: Span,
    },
    MacroCloseExplicit {
        macro_start: Span,
        macro_end: Span,
        macro_body: Option<Span>,
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
            Diagnostic::UnfinishedMacro {
                cur_start_span,
                cur_macro_span,
                file_end_span,
            } => build(file_end_span, "unfinished macro")
                .with_label(label!(cur_start_span, "macro started here"))
                .with_labels(
                    cur_macro_span
                        .as_ref()
                        .map(|span| label!(span, "macro body").with_priority(-1)),
                ),
            Diagnostic::MacroCloseExplicit {
                macro_start,
                macro_end,
                macro_body,
            } => build(macro_end, "unexpected macro closure")
                .with_label(label!(macro_start, "auto-closing macro started here"))
                .with_label(label!(macro_end, "closed explicitly here"))
                .with_labels(
                    macro_body
                        .as_ref()
                        .map(|span| label!(span, "macro body").with_priority(-1)),
                ),
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
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum Mode<M = MacroMode> {
    /// No open `code` environment.
    #[default]
    None,

    /// Inside a `code` environment with option `[hide]`.
    Hide,

    /// Inside of a macro definition.
    ///
    /// Depending on the context (ie. when stored in [`MacroMode::inner_mode`]) this can also stand
    /// for inside a `code` environment that is to be typeset.
    Macro(M),
}

#[derive(Debug, Default, Clone)]
struct MacroMode {
    /// The span of the command that started this macro definition.
    start: Span,
    /// The `body` span encompasses all the lines since the start of the macro definition.
    body: Option<Span>,
    /// If this macro is declared to typeset it's code *inline*.
    inline: bool,
    /// If this macro was indicated to auto-close on the first empty line.
    auto_close: bool,
    /// Describes the which kind of `code` environment is currently open, if any.
    inner_mode: Mode<()>,
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

        let s = s.trim_start();
        let (name, rest) = s
            .split_inclusive_r(|c: char| !c.is_ascii_alphabetic())
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
