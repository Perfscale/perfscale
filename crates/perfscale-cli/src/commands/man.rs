//! `perfscale man` — print or install the bundled man page.
//!
//! The page (`man/perfscale.1` in the repo) is embedded into the binary, so
//! the manual is always available — including on Windows, where there is no
//! `man(1)` at all. `--install` writes the roff source into a man directory
//! so `man perfscale` finds it on Unix systems.

use std::path::{Path, PathBuf};

use crate::cli::ManArgs;
use crate::error::CliError;

/// The roff source of the man page, embedded at compile time.
const MAN_SOURCE: &str = include_str!("../../../../man/perfscale.1");

pub async fn run(args: ManArgs) -> Result<(), CliError> {
    if args.install {
        let dir = match args.dir.clone().or_else(default_man_dir) {
            Some(d) => d,
            None => {
                return Err(CliError::new("could not locate your home directory")
                    .hint("pass the target directory explicitly: perfscale man --install --dir <path>"))
            }
        };
        return install(&dir);
    }

    if args.raw {
        print!("{MAN_SOURCE}");
    } else {
        print!("{}", render_text(MAN_SOURCE));
    }
    Ok(())
}

/// Default per-user man directory (`~/.local/share/man/man1`), which man-db
/// on most Linux distributions indexes out of the box.
fn default_man_dir() -> Option<PathBuf> {
    default_man_dir_from(std::env::var_os("HOME").map(PathBuf::from).as_deref())
}

fn default_man_dir_from(home: Option<&Path>) -> Option<PathBuf> {
    home.map(|h| h.join(".local/share/man/man1"))
}

/// Install the man page into `dir` so `man perfscale` finds it.
#[cfg(not(windows))]
fn install(dir: &Path) -> Result<(), CliError> {
    std::fs::create_dir_all(dir).map_err(|e| {
        CliError::new(format!(
            "failed to create man directory '{}'",
            dir.display()
        ))
        .cause(e.to_string())
        .hint("pass --dir with a writable path (system dirs like /usr/local/share/man/man1 need elevated privileges)")
    })?;

    let dest = dir.join("perfscale.1");
    std::fs::write(&dest, MAN_SOURCE).map_err(|e| {
        CliError::new(format!(
            "failed to write man page to '{}'",
            dest.display()
        ))
        .cause(e.to_string())
        .hint("pass --dir with a writable path (system dirs like /usr/local/share/man/man1 need elevated privileges)")
    })?;
    eprintln!("[system] installed man page to {}", dest.display());

    // Best-effort index refresh for `man -k` — `man perfscale` works without
    // it, and mandb is absent on some systems (e.g. macOS).
    let _ = std::process::Command::new("mandb")
        .arg("-q")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    eprintln!("[system] verify with: man perfscale");
    if let Some(parent) = dir.parent() {
        eprintln!(
            "[system] if man can't find it, add the parent directory to MANPATH:\n         export MANPATH=\"{}:$MANPATH\"",
            parent.display()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn install(_dir: &Path) -> Result<(), CliError> {
    Err(CliError::new("man pages are not used on Windows")
        .hint("just run `perfscale man` — it prints the manual as plain text"))
}

// ---------------------------------------------------------------------------
// Minimal roff → plain-text renderer
// ---------------------------------------------------------------------------

/// Render the subset of roff our man page uses as readable plain text:
/// section headers at the margin, body indented 7 columns, `.TP` tag/body
/// pairs, fill-mode paragraphs wrapped to the terminal width, `.nf`/`.fi`
/// blocks verbatim, font macros and escapes stripped.
pub fn render_text(src: &str) -> String {
    let mut out = String::new();
    let mut fill: Vec<String> = Vec::new(); // pending paragraph words
    let mut base = 0usize; // body indent — 7 once inside the first .SH
    let mut rs = 0usize; // extra indent from .RS/.RE
    let mut extra = 0usize; // +7 for the body of a .TP pair
    let mut nofill = false;
    let mut tp_tag_next = false;

    for line in src.lines() {
        if line.starts_with(".\\\"") || line.starts_with(".TH") {
            continue;
        }
        if nofill {
            // Macros still execute in no-fill mode; only text lines are
            // output verbatim.
            match line {
                ".fi" => {
                    nofill = false;
                    continue;
                }
                ".RS" => {
                    rs += 4;
                    continue;
                }
                ".RE" => {
                    rs = rs.saturating_sub(4);
                    continue;
                }
                _ => {}
            }
            push_indented(&mut out, base + rs, &strip_escapes(line));
            continue;
        }

        match line {
            ".nf" => {
                flush_fill(&mut out, &mut fill, base + rs + extra);
                nofill = true;
                continue;
            }
            ".fi" => continue,
            ".RS" => {
                flush_fill(&mut out, &mut fill, base + rs + extra);
                rs += 4;
                continue;
            }
            ".RE" => {
                flush_fill(&mut out, &mut fill, base + rs + extra);
                rs = rs.saturating_sub(4);
                continue;
            }
            ".br" => {
                flush_fill(&mut out, &mut fill, base + rs + extra);
                continue;
            }
            ".PP" | ".IP" => {
                flush_fill(&mut out, &mut fill, base + rs + extra);
                out.push('\n');
                extra = 0;
                continue;
            }
            ".TP" => {
                flush_fill(&mut out, &mut fill, base + rs + extra);
                tp_tag_next = true;
                continue;
            }
            ".UE" => continue,
            _ => {}
        }

        if let Some(rest) = line.strip_prefix(".SH") {
            flush_fill(&mut out, &mut fill, base + rs + extra);
            out.push('\n');
            out.push_str(&unquote(rest.trim()));
            out.push('\n');
            base = 7;
            extra = 0;
            continue;
        }
        if let Some(rest) = line.strip_prefix(".SS") {
            flush_fill(&mut out, &mut fill, base + rs + extra);
            out.push('\n');
            push_indented(&mut out, base + rs, &unquote(rest.trim()));
            extra = 0;
            continue;
        }
        if let Some(url) = line.strip_prefix(".UR") {
            if !url.trim().is_empty() {
                fill.push(url.trim().to_string());
            }
            continue;
        }

        let text = render_font_macro(line).unwrap_or_else(|| strip_escapes(line));
        if text.is_empty() {
            continue;
        }
        if tp_tag_next {
            // The tag of a .TP pair sits alone at the margin.
            push_indented(&mut out, base + rs, &text);
            tp_tag_next = false;
            extra = 7;
        } else {
            fill.push(text);
        }
    }
    flush_fill(&mut out, &mut fill, base + rs + extra);

    out.push('\n');
    out
}

fn push_indented(out: &mut String, indent: usize, text: &str) {
    if text.is_empty() {
        out.push('\n');
        return;
    }
    out.push_str(&" ".repeat(indent));
    out.push_str(text);
    out.push('\n');
}

/// Emit the pending paragraph: join the pieces with spaces and wrap greedily
/// to the fill width at the given indent.
fn flush_fill(out: &mut String, fill: &mut Vec<String>, indent: usize) {
    if fill.is_empty() {
        return;
    }
    let words: Vec<String> = fill
        .drain(..)
        .flat_map(|piece| piece.split_whitespace().map(str::to_owned).collect::<Vec<_>>())
        .collect();
    let width = 78usize.saturating_sub(indent).max(20);

    let mut col = indent;
    for (i, word) in words.iter().enumerate() {
        let wlen = word.chars().count();
        if i > 0 && col + 1 + wlen > indent + width {
            out.push('\n');
            col = indent;
        }
        if col == indent {
            out.push_str(&" ".repeat(indent));
        } else {
            out.push(' ');
            col += 1;
        }
        out.push_str(word);
        col += wlen;
    }
    out.push('\n');
}

/// Render a line-start font macro (`.B`, `.I`, `.BI`, `.BR`, `.IB`, `.IR`,
/// `.RB`, `.RI`). Same-font macros join their arguments with a space;
/// alternating-font macros concatenate them (spacing comes from quoted args,
/// e.g. `.BI \-\-k6 " FILE.js"` → `--k6 FILE.js`).
fn render_font_macro(line: &str) -> Option<String> {
    const MACROS: [(&str, bool); 8] = [
        (".BI", true),
        (".BR", true),
        (".IB", true),
        (".IR", true),
        (".RB", true),
        (".RI", true),
        (".B", false),
        (".I", false),
    ];
    for (prefix, alternating) in MACROS {
        let Some(rest) = line.strip_prefix(prefix) else {
            continue;
        };
        if rest.is_empty() {
            return Some(String::new());
        }
        if !rest.starts_with(' ') {
            continue; // ".Bold" is a different macro we don't know
        }
        let tokens = tokenize(rest.trim());
        let joined = if alternating {
            tokens.concat()
        } else {
            tokens.join(" ")
        };
        return Some(strip_escapes(&joined));
    }
    None
}

/// Split macro arguments into tokens, honoring double-quoted spans. A quote
/// opens a span only at the start of a token (after whitespace); embedded
/// quotes like `{"lines":` stay literal.
fn tokenize(rest: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            '"' if cur.is_empty() => {
                for t in chars.by_ref() {
                    if t == '"' {
                        break;
                    }
                    cur.push(t);
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// Strip roff escapes we use: `\fX`/`\f(XX` font switches, `\-`, `\e`,
/// `\(em`. Anything unknown keeps its literal character.
fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('f') => match chars.peek() {
                Some('(') => {
                    chars.next();
                    chars.next();
                    chars.next();
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            Some('-') => out.push('-'),
            Some('e') => out.push('\\'),
            Some('(') => {
                let a = chars.next();
                let b = chars.next();
                if let (Some('e'), Some('m')) = (a, b) {
                    out.push_str("--");
                }
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    let t = t
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(t);
    strip_escapes(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_source_is_the_man_page() {
        assert!(MAN_SOURCE.starts_with(".\\\" perfscale(1)"));
        assert!(MAN_SOURCE.contains(".TH PERFSCALE 1"));
        assert!(MAN_SOURCE.contains(".SH NAME"));
    }

    #[test]
    fn renders_section_headers_at_margin_and_body_indented() {
        let text = render_text(MAN_SOURCE);
        assert!(text.contains("\nNAME\n"), "header at column 0:\n{text}");
        assert!(
            text.contains("\n       perfscale - run k6"),
            "body indented 7:\n{text}"
        );
        assert!(text.contains("\nRUN OPTIONS\n"));
        assert!(text.contains("\nMAN OPTIONS\n"));
    }

    #[test]
    fn renders_tp_tag_and_indented_body() {
        let text = render_text(".SH TEST\n.TP\n.B run\nRun a load test.\n");
        assert!(text.contains("       run\n"), "tag at body indent:\n{text}");
        assert!(
            text.contains("              Run a load test.\n"),
            "body indented +7:\n{text}"
        );
    }

    #[test]
    fn font_macros_concatenate_tokens_and_strip_escapes() {
        assert_eq!(render_font_macro(".B perfscale"), Some("perfscale".into()));
        assert_eq!(
            render_font_macro(".BI \\-\\-k6 \" FILE.js\""),
            Some("--k6 FILE.js".into())
        );
        assert_eq!(render_font_macro(".BR \\-\\-k6 ,"), Some("--k6,".into()));
        // roff alternates fonts per token and concatenates — `< COMMAND >`
        // becomes `<COMMAND>` in real man output too.
        assert_eq!(render_font_macro(".RI < COMMAND >"), Some("<COMMAND>".into()));
        assert_eq!(render_font_macro(".Bold x"), None);
    }

    #[test]
    fn strips_escapes_and_maps_special_chars() {
        assert_eq!(strip_escapes("\\-\\-summary\\-export"), "--summary-export");
        assert_eq!(strip_escapes("\\fBbold\\fR plain"), "bold plain");
        assert_eq!(strip_escapes("handy \\(em nice"), "handy -- nice");
        assert_eq!(strip_escapes("\\f(CWcode\\fR"), "code");
    }

    #[test]
    fn nofill_blocks_stay_verbatim() {
        let text = render_text(".SH EXAMPLES\n.PP\n.nf\n.RS\nperfscale run --k6 a.js\n.RE\n.fi\n");
        assert!(
            text.contains("           perfscale run --k6 a.js\n"),
            "verbatim with base+RS indent:\n{text}"
        );
    }

    #[test]
    fn rendered_page_has_every_section() {
        let text = render_text(MAN_SOURCE);
        for section in [
            "NAME",
            "SYNOPSIS",
            "DESCRIPTION",
            "COMMANDS",
            "RUN OPTIONS",
            "MAN OPTIONS",
            "ENVIRONMENT",
            "EXIT STATUS",
            "EXAMPLES",
            "SEE ALSO",
        ] {
            assert!(text.contains(section), "missing {section}");
        }
        assert!(!text.contains("\\f"), "font escapes stripped:\n{text}");
        assert!(!text.contains(".SH"), "macros consumed:\n{text}");
    }

    #[test]
    fn default_man_dir_is_per_user_man1() {
        let dir = default_man_dir_from(Some(Path::new("/home/alice"))).unwrap();
        assert_eq!(dir, PathBuf::from("/home/alice/.local/share/man/man1"));
        assert!(default_man_dir_from(None).is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn install_writes_the_page() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("man1");
        install(&target).unwrap();
        let written = std::fs::read_to_string(target.join("perfscale.1")).unwrap();
        assert_eq!(written, MAN_SOURCE);
    }
}
