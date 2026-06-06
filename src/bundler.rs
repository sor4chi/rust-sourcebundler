// Copyright (C) 2022 Leandro Lisboa Penz <lpenz@lpenz.org>
// This file is subject to the terms and conditions defined in
// file 'LICENSE', which is part of this source code package.

use std::collections::HashSet;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use cargo_toml;
use lazy_static::lazy_static;
use regex::Regex;

const LIBRS_FILENAME: &str = "src/lib.rs";
lazy_static! {
    static ref WARN_RE: Regex = source_line_regex(r" #!\[warn\(.*").unwrap();
    // The lib is wrapped in `pub mod <crate_name>`, so absolute `crate::` paths inside
    // the lib are rewritten to `crate::<crate_name>::`. This is correct for both `use`
    // statements and inline expression paths, regardless of module nesting depth.
    // The optional first segment and trailing `!` let us special-case macro invocations:
    // `crate::NAME!` is a `#[macro_export]` macro, which stays at the bundle's crate root.
    static ref CRATE_PATH_RE: Regex = Regex::new(r"\bcrate::(?P<seg>\w+)?(?P<bang>!)?").unwrap();
    static ref MINIFY_RE: Regex = Regex::new(r"^\s*(?P<contents>.*)\s*$").unwrap();
}

pub struct Bundler<'a> {
    binrs_filename: &'a Path,
    bundle_filename: Option<&'a Path>,
    bundle_file: Box<dyn Write>,
    basedir: PathBuf,
    _crate_name: String,
    skip_use: HashSet<String>,
    minify: bool,
    /// Whether the line currently being processed comes from the lib (rewrites crate:: when true).
    in_lib: bool,
    /// Whether to strip comments from the output (useful to avoid leaking notes in submissions).
    strip_comments: bool,
    /// Tracks multi-line `/* */` block-comment state while stripping comments.
    in_block: bool,
    /// While true, subsequent `//` comment lines are preserved verbatim despite `strip_comments`
    /// (entered by a lone `// bundle-keep` marker line; the marker itself is dropped).
    keep_block: bool,
}

/// Defines a regex to match a line of rust source.
/// Uses a shorthand where "  " = "\s+" and " " = "\s*"
fn source_line_regex<S: AsRef<str>>(source_regex: S) -> Result<Regex> {
    Regex::new(
        format!(
            "^{}(?://.*)?$",
            source_regex
                .as_ref()
                .replace("  ", r"\s+")
                .replace(' ', r"\s*")
        )
        .as_str(),
    )
    .map_err(|e| anyhow!(e))
}

impl<'a> Bundler<'a> {
    pub fn new(binrs_filename: &'a Path, bundle_filename: &'a Path) -> Bundler<'a> {
        let mut bundler = Self::new_fd(
            binrs_filename,
            Box::new(BufWriter::new(File::create(bundle_filename).unwrap())),
        );
        bundler.bundle_filename = Some(bundle_filename);
        bundler
    }

    pub fn new_fd(binrs_filename: &'a Path, bundle_file: Box<dyn Write>) -> Bundler<'a> {
        Bundler {
            binrs_filename,
            bundle_filename: None,
            bundle_file,
            basedir: PathBuf::default(),
            _crate_name: String::from(""),
            skip_use: HashSet::new(),
            minify: false,
            in_lib: false,
            strip_comments: false,
            in_block: false,
            keep_block: false,
        }
    }

    pub fn minify_set(&mut self, enable: bool) {
        self.minify = enable;
    }

    /// When enabled, `//` line comments and `/* */` block comments are removed
    /// from the output (string, char, and raw-string literals are preserved).
    pub fn strip_comments_set(&mut self, enable: bool) {
        self.strip_comments = enable;
    }

    pub fn crate_name(&mut self, name: &'a str) {
        self._crate_name = String::from(name);
    }

    fn do_run(&mut self) -> Result<()> {
        let canon_binrs = self.binrs_filename.canonicalize().with_context(|| {
            format!(
                "error canonicalizing binrs dir [{}]",
                self.binrs_filename.display()
            )
        })?;
        self.basedir = PathBuf::from(
            canon_binrs
                .ancestors()
                .find(|a| a.join("Cargo.toml").is_file())
                .ok_or_else(|| {
                    anyhow!(
                        "could not find Cargo.toml in ancestors of [{}]",
                        canon_binrs.display()
                    )
                })?,
        );
        let cargo_filename = self.basedir.join("Cargo.toml");
        let cargo = cargo_toml::Manifest::from_path(&cargo_filename)
            .with_context(|| format!("error parsing {}", cargo_filename.display()))?;
        self._crate_name = cargo
            .package
            .ok_or_else(|| {
                anyhow!(
                    "Could not get crate name from [{}]",
                    cargo_filename.display()
                )
            })?
            .name
            .replace('-', "_");
        self.binrs()
            .with_context(|| format!("error building bin.rs {}", self.binrs_filename.display()))?;
        if let Some(bundle_filename) = self.bundle_filename {
            println!("rerun-if-changed={}", bundle_filename.display());
        }
        self.bundle_file.flush().with_context(|| {
            format!(
                "error while flushing bundle_file {:?}",
                self.bundle_filename
            )
        })?;
        Ok(())
    }

    pub fn run(mut self) {
        self.do_run().unwrap();
    }

    /// From the file that has the main() function, expand "extern
    /// crate <_crate_name>" into lib.rs contents, and smartly skips
    /// "use <_crate_name>::" lines.
    fn binrs(&mut self) -> Result<()> {
        let bin_fd = File::open(self.binrs_filename)?;
        let mut bin_reader = BufReader::new(&bin_fd);

        let extcrate_re = source_line_regex(format!(r" extern  crate  {} ; ", self._crate_name))?;
        let useselfcrate_re =
            source_line_regex(format!(r" use  (?P<submod>{}::.*) ; ", self._crate_name))?;

        let mut line = String::new();
        while bin_reader.read_line(&mut line)? > 0 {
            line.truncate(line.trim_end().len());
            if WARN_RE.is_match(&line) {
            } else if extcrate_re.is_match(&line) {
                writeln!(self.bundle_file, "pub mod {} {{", self._crate_name)?;
                self.librs()?;
                writeln!(self.bundle_file, "}}")?;
            } else if let Some(cap) = useselfcrate_re.captures(&line) {
                let submod = cap
                    .name("submod")
                    .ok_or_else(|| anyhow!("capture not found"))?
                    .as_str();
                writeln!(self.bundle_file, "use self::{};", submod)?;
            } else {
                self.write_line(&line)?;
            }
            line.clear();
        }
        Ok(())
    }

    /// Expand lib.rs contents and "pub mod <>;" lines.
    fn librs(&mut self) -> Result<()> {
        let lib_fd = File::open(self.basedir.join(LIBRS_FILENAME))?;
        let mut lib_reader = BufReader::new(&lib_fd);

        let mod_re = source_line_regex(r" (pub  )?mod  (?P<m>.+) ; ")?;

        self.in_lib = true;
        let mut line = String::new();
        while lib_reader.read_line(&mut line)? > 0 {
            line.pop();
            if WARN_RE.is_match(&line) {
            } else if let Some(cap) = mod_re.captures(&line) {
                let modname = cap
                    .name("m")
                    .ok_or_else(|| anyhow!("capture not found"))?
                    .as_str();
                if modname != "tests" {
                    self.usemod(modname, modname, modname)?;
                }
            } else {
                self.write_line(&line)?;
            }
            line.clear(); // clear to reuse the buffer
        }
        self.in_lib = false;
        Ok(())
    }

    /// Called to expand random .rs files from lib.rs. It recursivelly
    /// expands further "pub mod <>;" lines and updates the list of
    /// "use <>;" lines that have to be skipped.
    fn usemod(&mut self, mod_name: &str, mod_path: &str, mod_import: &str) -> Result<()> {
        let mod_filenames0 = [
            format!("src/{}.rs", mod_path),
            format!("src/{}/mod.rs", mod_path),
        ];
        let mod_fd = mod_filenames0
            .iter()
            .map(|fn0| {
                let mod_filename = self.basedir.join(fn0);
                File::open(mod_filename)
            })
            .find(|fd| fd.is_ok())
            .ok_or_else(|| anyhow!("no mod file found"))??;
        let mut mod_reader = BufReader::new(mod_fd);

        let mod_re = source_line_regex(r" (pub  )?mod  (?P<m>.+) ; ")?;

        let mut line = String::new();

        writeln!(self.bundle_file, "pub mod {} {{", mod_name)?;
        self.skip_use.insert(String::from(mod_import));

        while mod_reader.read_line(&mut line)? > 0 {
            line.truncate(line.trim_end().len());
            if WARN_RE.is_match(&line) {
            } else if let Some(cap) = mod_re.captures(&line) {
                let submodname = cap
                    .name("m")
                    .ok_or_else(|| anyhow!("capture not found"))?
                    .as_str();
                if submodname != "tests" {
                    let submodfile = format!("{}/{}", mod_path, submodname);
                    let submodimport = format!("{}::{}", mod_import, submodname);
                    self.usemod(submodname, submodfile.as_str(), submodimport.as_str())?;
                }
            } else {
                self.write_line(&line)?;
            }
            line.clear(); // clear to reuse the buffer
        }

        writeln!(self.bundle_file, "}}")?;

        Ok(())
    }

    fn write_line(&mut self, line: &str) -> Result<()> {
        // Credit/license preservation: a lone `// bundle-keep` marker enters "keep mode" in which
        // the following `//` comment lines are kept verbatim despite strip_comments. The marker
        // line itself is dropped from the output. Keep mode ends at the first non-comment line.
        if self.strip_comments {
            let trimmed = line.trim_start();
            if trimmed == "// bundle-keep" {
                self.keep_block = true;
                return Ok(());
            }
            if self.keep_block {
                if trimmed.starts_with("//") {
                    return self.emit_line(line);
                }
                self.keep_block = false;
            }
        }
        // Optionally strip comments. Drop lines that become empty (full-line comments).
        let stripped;
        let line = if self.strip_comments {
            stripped = self.strip_comments_in_line(line);
            if stripped.trim().is_empty() {
                return Ok(());
            }
            stripped.as_str()
        } else {
            line
        };
        self.emit_line(line)
    }

    /// Apply crate:: rewriting and minify, then write the line. Comment handling already done.
    fn emit_line(&mut self, line: &str) -> Result<()> {
        // Rewrite absolute crate:: paths inside the lib to crate::<crate_name>::.
        let cow = if self.in_lib {
            std::borrow::Cow::Owned(rewrite_crate_paths(line, &self._crate_name))
        } else {
            std::borrow::Cow::Borrowed(line)
        };
        let line: &str = &cow;
        if self.minify {
            writeln!(
                self.bundle_file,
                "{}",
                MINIFY_RE.replace_all(line, "$contents")
            )
        } else {
            writeln!(self.bundle_file, "{}", line)
        }
        .map_err(|e| anyhow!(e))
    }

    /// Remove `//` line comments and `/* */` block comments from a single line,
    /// while preserving string, char, byte-string and raw-string literals.
    /// `self.in_block` carries block-comment state across lines.
    ///
    /// Limitation: a raw string that spans multiple lines is not tracked across
    /// lines (single-line raw strings, the common case, are handled correctly).
    fn strip_comments_in_line(&mut self, line: &str) -> String {
        let chars: Vec<char> = line.chars().collect();
        let n = chars.len();
        let mut out = String::with_capacity(line.len());
        let mut i = 0;
        while i < n {
            if self.in_block {
                if chars[i] == '*' && i + 1 < n && chars[i + 1] == '/' {
                    self.in_block = false;
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            let c = chars[i];
            // Line comment: the rest of the line is dropped.
            if c == '/' && i + 1 < n && chars[i + 1] == '/' {
                break;
            }
            // Block comment start.
            if c == '/' && i + 1 < n && chars[i + 1] == '*' {
                self.in_block = true;
                i += 2;
                continue;
            }
            // Raw string: r"...", r#"..."#, br"...", etc.
            if (c == 'r' || c == 'b') && !is_ident_char(prev_char(&chars, i)) {
                if let Some(next) = copy_raw_string(&chars, i, &mut out) {
                    i = next;
                    continue;
                }
            }
            // Normal string / byte string literal.
            if c == '"' {
                out.push(c);
                i += 1;
                while i < n {
                    let d = chars[i];
                    out.push(d);
                    i += 1;
                    if d == '\\' && i < n {
                        out.push(chars[i]);
                        i += 1;
                    } else if d == '"' {
                        break;
                    }
                }
                continue;
            }
            // Char literal vs lifetime/label.
            if c == '\'' {
                if i + 1 < n && chars[i + 1] == '\\' {
                    // Escaped char literal: '\n', '\'', '\u{..}', ...
                    out.push(c);
                    i += 1;
                    while i < n {
                        let d = chars[i];
                        out.push(d);
                        i += 1;
                        if d == '\\' && i < n {
                            out.push(chars[i]);
                            i += 1;
                        } else if d == '\'' {
                            break;
                        }
                    }
                    continue;
                }
                if i + 2 < n && chars[i + 2] == '\'' {
                    // Simple char literal: 'x', '"', '/' ...
                    out.push(chars[i]);
                    out.push(chars[i + 1]);
                    out.push(chars[i + 2]);
                    i += 3;
                    continue;
                }
                // Otherwise a lifetime/label ('a, 'static): treat ' as ordinary.
                out.push(c);
                i += 1;
                continue;
            }
            out.push(c);
            i += 1;
        }
        out
    }
}

/// The char before `i`, or a space when at the start of the line.
fn prev_char(chars: &[char], i: usize) -> char {
    if i == 0 {
        ' '
    } else {
        chars[i - 1]
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Rewrite absolute `crate::` paths from inside the lib so they resolve in the
/// bundled crate, where the lib is nested under `pub mod <crate_name>`.
///
/// Ordinary item paths gain the crate-name prefix (`crate::X` -> `crate::<name>::X`),
/// but `#[macro_export]` macro invocations (`crate::NAME!`) are left untouched:
/// exported macros are always hoisted to the crate root, so they live at
/// `crate::NAME!` in the bundle too and must not be prefixed.
fn rewrite_crate_paths(line: &str, crate_name: &str) -> String {
    CRATE_PATH_RE
        .replace_all(line, |caps: &regex::Captures| {
            match (caps.name("seg"), caps.name("bang")) {
                // `crate::NAME!` -> macro_export invocation, keep at crate root.
                (Some(seg), Some(_)) => format!("crate::{}!", seg.as_str()),
                // `crate::seg...` -> prefix the (already consumed) first segment.
                (Some(seg), None) => format!("crate::{}::{}", crate_name, seg.as_str()),
                // Bare `crate::` (e.g. `crate::{a, b}`, `crate::*`): just prefix.
                _ => format!("crate::{}::", crate_name),
            }
        })
        .into_owned()
}

/// If a raw string literal (`r"..."`, `r#"..."#`, `br"..."`, ...) starts at
/// `start`, copy it verbatim into `out` and return the index just past it.
/// Returns `None` when there is no raw string at `start` (e.g. a raw identifier
/// `r#ident`, or a plain `r`/`b` in code).
fn copy_raw_string(chars: &[char], start: usize, out: &mut String) -> Option<usize> {
    let n = chars.len();
    let mut i = start;
    if chars[i] == 'b' {
        if i + 1 >= n || chars[i + 1] != 'r' {
            return None;
        }
        i += 1;
    }
    if chars[i] != 'r' {
        return None;
    }
    let mut j = i + 1;
    let mut hashes = 0;
    while j < n && chars[j] == '#' {
        hashes += 1;
        j += 1;
    }
    if j >= n || chars[j] != '"' {
        return None; // raw identifier or just a letter, not a raw string.
    }
    for &ch in &chars[start..=j] {
        out.push(ch);
    }
    let mut k = j + 1;
    while k < n {
        if chars[k] == '"' {
            let closing = (1..=hashes).all(|h| k + h < n && chars[k + h] == '#');
            if closing {
                out.push('"');
                for _ in 0..hashes {
                    out.push('#');
                }
                return Some(k + 1 + hashes);
            }
        }
        out.push(chars[k]);
        k += 1;
    }
    Some(k) // unterminated on this line (multi-line raw strings are not tracked).
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn strip(line: &str) -> String {
        let mut b = Bundler::new_fd(Path::new("x"), Box::new(Vec::new()));
        b.strip_comments_in_line(line)
    }

    #[test]
    fn strips_full_line_and_trailing_comments() {
        assert_eq!(strip("    // a note").trim(), "");
        assert_eq!(strip("let x = 1; // trailing"), "let x = 1; ");
        assert_eq!(strip("/// doc").trim(), "");
        assert_eq!(strip("//! inner doc").trim(), "");
    }

    #[test]
    fn keeps_comment_markers_inside_strings() {
        assert_eq!(strip(r#"let u = "http://x"; // c"#), r#"let u = "http://x"; "#);
        assert_eq!(strip(r#"let s = "a // b /* c";"#), r#"let s = "a // b /* c";"#);
    }

    #[test]
    fn keeps_char_literals_and_lifetimes() {
        // '"' is a char literal holding a double quote; must not start a string.
        assert_eq!(strip(r#"let c = '"'; // x"#), r#"let c = '"'; "#);
        assert_eq!(strip("let c = '/'; // x"), "let c = '/'; ");
        // Lifetimes must not be treated as char literals.
        assert_eq!(strip("fn f<'a>(x: &'a str) {} // x"), "fn f<'a>(x: &'a str) {} ");
        assert_eq!(strip("T: Fn() + 'static // x"), "T: Fn() + 'static ");
    }

    #[test]
    fn keeps_raw_strings() {
        assert_eq!(strip(r##"let r = r"a//b"; // c"##), r##"let r = r"a//b"; "##);
        assert_eq!(strip(r####"let r = r#"a"//b"#; // c"####), r####"let r = r#"a"//b"#; "####);
    }

    #[test]
    fn handles_block_comments_across_lines() {
        let mut b = Bundler::new_fd(Path::new("x"), Box::new(Vec::new()));
        assert_eq!(b.strip_comments_in_line("code; /* start").trim(), "code;");
        assert_eq!(b.strip_comments_in_line("still in comment").trim(), "");
        assert_eq!(b.strip_comments_in_line("end */ tail").trim(), "tail");
    }

    #[test]
    fn rewrites_item_paths_but_not_macro_export_invocations() {
        // Ordinary item paths get the crate-name prefix, at any nesting depth.
        assert_eq!(rewrite_crate_paths("use crate::foo::Bar;", "mylib"), "use crate::mylib::foo::Bar;");
        assert_eq!(rewrite_crate_paths("$crate::a::B", "mylib"), "$crate::mylib::a::B");
        // Brace / glob imports keep working (no first ident segment).
        assert_eq!(rewrite_crate_paths("use crate::{a, b};", "mylib"), "use crate::mylib::{a, b};");
        assert_eq!(rewrite_crate_paths("use crate::*;", "mylib"), "use crate::mylib::*;");
        // `#[macro_export]` macro invocations stay at the crate root (no prefix).
        assert_eq!(rewrite_crate_paths("let g = crate::my_macro!(A => 1.0);", "mylib"), "let g = crate::my_macro!(A => 1.0);");
        // Non-crate paths are untouched (word boundary guards `mycrate`).
        assert_eq!(rewrite_crate_paths("use mycrate::foo;", "mylib"), "use mycrate::foo;");
    }

    #[test]
    fn bundle_keep_preserves_credit_and_drops_marker() {
        use std::cell::RefCell;
        use std::rc::Rc;
        struct SharedBuf(Rc<RefCell<Vec<u8>>>);
        impl std::io::Write for SharedBuf {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Rc::new(RefCell::new(Vec::new()));
        let mut b = Bundler::new_fd(Path::new("x"), Box::new(SharedBuf(buf.clone())));
        b.strip_comments_set(true);
        for l in [
            "// bundle-keep",
            "// based on the original Foo library",
            "// (c) 2020 Example Author",
            "let x = 1; // strip this trailing note",
            "// a normal note after the block",
        ] {
            b.write_line(l).unwrap();
        }
        let out = String::from_utf8(buf.borrow().clone()).unwrap();
        assert!(!out.contains("bundle-keep"), "marker line must be dropped");
        assert!(out.contains("// based on the original Foo library"));
        assert!(out.contains("// (c) 2020 Example Author"));
        assert!(out.contains("let x = 1;"), "code line kept");
        assert!(!out.contains("strip this trailing note"), "keep mode ends at code line");
        assert!(!out.contains("a normal note after the block"), "later normal comment stripped");
    }
}
