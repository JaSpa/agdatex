# agdatex

`agdatex` allows to extract reusable LaTeX-Macros from `.agda`-files
annotated via comments.

## Dependencies

- Python >= 3.11

## Annotations

The following comment annotations are supported:

-   `--! NAME` defines the `\NAME` LaTeX-macro for the Agda-code from the next
    line until the next empty line or EOF.
    Example:
    ```agda
    --! example
    ex : ℕ
    ex = 0

    foo : ℕ
    foo = 1
    ```
    Defines the `\example` macro, which includes `ex` but not `foo`.

-   `--! NAME {` defines the `\NAME` LaTeX-macro for the Agda-code from the next
    line until the `--! }` end annotation token.
    Example:
    ```agda
    --! examples {
    ex : ℕ
    ex = 0

    foo : ℕ
    foo = 1
    --! }
    ```
    Defines the `\examples` macro, which includes `ex` and `foo`.

-   `--! NAME >` opens a namespace, i.e. all subsequent definitions will be
    prefixed with `NAME`. Namespaces can be nested.
    Example:
    ```agda
    --! Example >

    --! Ex
    ex : ℕ
    ex = 0

    --! Foo
    foo : ℕ
    foo = 1
    ```
    Defines the macros `\ExampleEx` and `\ExampleFoo`.

-   `--! <` closes a namespace. Namespaces are also automatically
    closed at the end of the file.

-   `--!! NAME` behaves like `--!` but the resulting macro typesets
    the code inline and not as a block.

-   `--! [` hides code inside of a command.

-   `--! ]` stops hiding code inside of a command.

## Alignment between multiple code blocks

The code typeset by a single extracted macro is wrapped in an `AgdaAlign`
environment. This is necessary to ensure proper alignment if segments inside a
single macro are hidden using `--! [` and `--! ]`.

But to align code from multiple macros a surrounding `AgdaAlign` environment is
required. `AgdaAlign` environments, however, (or similar environments, such as
`AgdaMultiCode`) may not be nested. Instead, use the starred version of the
extracted macro `\Foo*` which _does not_ wrap the code in
`AgdaAlign`/`AgdaSuppressSpace` but makes this your reponsibility.

```latex
\begin{AgdaAlign}
\FunctionPartI*
... explanation/intermediate prose ...
\FunctionPartII*
\end{AgdaAlign}
```

In some cases the starred version may also provide a workaround for [`! Package
array Error: Empty preamble: 'l' used.`][issue 7290]. If LaTeX emits this error
for a macro which does not require an explicit `AgdaAlign` environment (ie. no
`--! [` and `--! ]`) you can try using the starred version of the macro.

For further information regarding [alignment][] and [multiple code blocks][]
please refer to the Agda documentation.

[alignment]: https://agda.readthedocs.io/en/stable/tools/generating-latex.html#alignment
[multiple code blocks]: https://agda.readthedocs.io/en/stable/tools/generating-latex.html#breaking-up-code-blocks
[issue 7290]: https://github.com/agda/agda/issues/7290

## Usage

Let's say you have a project structure like

```
.
├── .git
├── agda.sty
├── main.tex
└── STLC
    ├── Properties.agda
    └── Syntax.agda
```

where `STLC/Syntax.agda` begins with
```agda
module STLC.Syntax where
```

Running
```
$ ./agdatex.py -o latex STLC/*.agda
```
has the following effect:

-   the project root is discovered by searching outwards until a
    `.git` directory is found;

-   the project root is copied to a fresh temp directory;

-   the copied `STLC/*.agda`-files are transpiled to
    `*.lagda.tex`-files;

-   agda is run with the LaTeX-backend on the `*.lagda.tex`-files;

-   the corresponding `latex/STLC/*.tex` files appear in the actual
    project root;

-   a `latex/agda-generated.sty` file is generated that `\input`s all the
    generated `STLC/*.tex` files;

-   the temp directory is removed again.

The `main.tex`-file can then import all generated macros via
`\usepackage{agda-generated}`.

Copying is necessary as Agda does not support having `.lagda.tex`- and
`.agda`-files with the same name. Copying the entire project root
allows the `.agda`-files, which should be converted to LaTeX, to
dependent on the rest of the project.

For smaller projects without a `.agda-lib`-file, you might want to
explicitly specify the root directory via the `--root` flag.

## Complete Example

A complete example including a `Makefile` can be found in the
`example`-subdirectory.

## Manpage

Usage: `agdatex [-h] [-o PATH] [-e PATH] [-t PATH] [-k] [-r PATH] [-i PATH] SRC_PATH [SRC_PATH ...]`

Positional Arguments:

-   `SRC_PATH`&nbsp;&nbsp;
    Path to an annotated `.agda`-file.

Optional Arguments:

-   `-o PATH, --outputdir PATH`&nbsp;&nbsp;
    Output directory for agda's LaTeX backend. Forwarded as
    `--latex-dir` to agda. Default: `latex`.

-   `-e PATH, --exportfile PATH`&nbsp;&nbsp;
    This file will `\\input` all generated `.tex`-files. Both `.tex`
    and `.sty` are supported. Default: `OUTPUTDIR/agda-generated.sty`.

-   `-t PATH, --tempdir PATH`&nbsp;&nbsp;
    Temporary directory to copy the project root to. Default: fresh
    system-dependent temporary dir.

-   `-k, --keeptempdir`&nbsp;&nbsp;
    Keep temporary directory for debugging.

-   `-r PATH, --root PATH`&nbsp;&nbsp;
    Project root. Default: Search for `.git`-directory.

-   `-i PATH, --index PATH`&nbsp;&nbsp;
    Write the list of generated macros to this file.


