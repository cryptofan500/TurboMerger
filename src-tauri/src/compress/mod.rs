//! Compress: tree-sitter-based content reduction (T2-3 / T2-4).
//!
//! Two independent transforms. Both are graceful: an unsupported extension, a
//! parse with errors, or a no-op transform returns `None` and the caller keeps
//! the original content — compression must never corrupt a merge.
//!
//! - [`compress_signatures`]: elide function/method bodies (keep signatures,
//!   types, imports, class structure). Bodies become `{ ... }` (brace
//!   languages) or `...` (Python).
//! - [`strip_comments`]: remove comment nodes; whole-line comments take their
//!   line with them so the output isn't riddled with blank lines.

use tree_sitter::{Language, Node, Parser};

/// Language family: decides node kinds and the body replacement text.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Rust,
    JsTs,
    Python,
    Go,
    Java,
    C,
    Cpp,
}

fn family_for_ext(ext: &str) -> Option<(Family, Language)> {
    Some(match ext {
        "rs" => (Family::Rust, tree_sitter_rust::LANGUAGE.into()),
        "js" | "jsx" | "mjs" | "cjs" => (Family::JsTs, tree_sitter_javascript::LANGUAGE.into()),
        "ts" | "mts" | "cts" => (
            Family::JsTs,
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        ),
        "tsx" => (Family::JsTs, tree_sitter_typescript::LANGUAGE_TSX.into()),
        "py" | "pyi" => (Family::Python, tree_sitter_python::LANGUAGE.into()),
        "go" => (Family::Go, tree_sitter_go::LANGUAGE.into()),
        "java" => (Family::Java, tree_sitter_java::LANGUAGE.into()),
        "c" | "h" => (Family::C, tree_sitter_c::LANGUAGE.into()),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => {
            (Family::Cpp, tree_sitter_cpp::LANGUAGE.into())
        }
        _ => return None,
    })
}

/// Node kinds whose `body` field is a candidate for elision.
fn body_owner_kinds(family: Family) -> &'static [&'static str] {
    match family {
        Family::Rust => &["function_item"],
        Family::JsTs => &[
            "function_declaration",
            "function_expression",
            "generator_function_declaration",
            "generator_function",
            "method_definition",
            "arrow_function",
        ],
        Family::Python => &["function_definition"],
        Family::Go => &["function_declaration", "method_declaration", "func_literal"],
        Family::Java => &["method_declaration", "constructor_declaration"],
        Family::C | Family::Cpp => &["function_definition"],
    }
}

/// Only elide block-shaped bodies (never expression bodies like `x => x * 2`).
const BLOCK_KINDS: &[&str] = &["block", "statement_block", "compound_statement"];

const COMMENT_KINDS: &[&str] = &["comment", "line_comment", "block_comment"];

fn parse(content: &str, language: &Language) -> Option<tree_sitter::Tree> {
    let mut parser = Parser::new();
    parser.set_language(language).ok()?;
    let tree = parser.parse(content, None)?;
    // A broken parse means byte ranges can't be trusted; keep the original.
    if tree.root_node().has_error() {
        return None;
    }
    Some(tree)
}

/// Replace function/method bodies with a placeholder, keeping signatures.
/// Returns `None` when the language is unsupported, the parse failed, or
/// nothing changed.
pub fn compress_signatures(content: &str, ext: &str) -> Option<String> {
    let (family, language) = family_for_ext(ext)?;
    let tree = parse(content, &language)?;
    let root = tree.root_node();
    let owners = body_owner_kinds(family);
    let replacement = if family == Family::Python {
        "..."
    } else {
        "{ ... }"
    };

    let mut edits: Vec<(std::ops::Range<usize>, &str)> = Vec::new();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let elided_body: Option<Node> = if owners.contains(&node.kind()) {
            node.child_by_field_name("body")
                .filter(|b| BLOCK_KINDS.contains(&b.kind()))
        } else {
            None
        };
        if let Some(body) = elided_body {
            edits.push((body.byte_range(), replacement));
        }
        for child in node.children(&mut cursor) {
            // Skip the elided body's subtree: nested functions vanish with it.
            if elided_body.is_some_and(|b| b.id() == child.id()) {
                continue;
            }
            stack.push(child);
        }
    }
    apply_edits(content, edits)
}

/// Remove comments. Whole-line comments consume their line (incl. newline);
/// trailing comments also eat the spaces that separated them from the code.
/// Returns `None` when unsupported, parse failed, or nothing changed.
pub fn strip_comments(content: &str, ext: &str) -> Option<String> {
    let (_, language) = family_for_ext(ext)?;
    let tree = parse(content, &language)?;
    let root = tree.root_node();

    let mut edits: Vec<(std::ops::Range<usize>, &str)> = Vec::new();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if COMMENT_KINDS.contains(&node.kind()) {
            let (start, end) = expand_comment_range(content, node.start_byte(), node.end_byte());
            edits.push((start..end, ""));
            continue;
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    apply_edits(content, edits)
}

/// Widen a comment's byte range: a comment alone on its line takes the whole
/// line; a trailing comment takes its preceding inline whitespace.
fn expand_comment_range(src: &str, start: usize, end: usize) -> (usize, usize) {
    let bytes = src.as_bytes();
    let mut line_start = start;
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }
    let only_ws_before = src[line_start..start]
        .chars()
        .all(|c| c == ' ' || c == '\t');
    if only_ws_before {
        let mut e = end;
        if e < bytes.len() && bytes[e] == b'\r' {
            e += 1;
        }
        if e < bytes.len() && bytes[e] == b'\n' {
            e += 1;
        }
        (line_start, e)
    } else {
        let mut s = start;
        while s > line_start && (bytes[s - 1] == b' ' || bytes[s - 1] == b'\t') {
            s -= 1;
        }
        (s, end)
    }
}

/// Splice non-overlapping edits into a fresh string. `None` if no edits.
fn apply_edits(src: &str, mut edits: Vec<(std::ops::Range<usize>, &str)>) -> Option<String> {
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|(r, _)| r.start);
    let mut out = String::with_capacity(src.len());
    let mut pos = 0usize;
    for (range, replacement) in edits {
        if range.start < pos {
            // Overlap (shouldn't happen by construction) — skip defensively.
            continue;
        }
        out.push_str(&src[pos..range.start]);
        out.push_str(replacement);
        pos = range.end;
    }
    out.push_str(&src[pos..]);
    if out == src {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_bodies_become_placeholders_structs_survive() {
        let src = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub struct Point {\n    pub x: i32,\n}\n\nimpl Point {\n    pub fn norm(&self) -> i32 {\n        self.x.abs()\n    }\n}\n";
        let out = compress_signatures(src, "rs").expect("changed");
        assert!(out.contains("pub fn add(a: i32, b: i32) -> i32 { ... }"));
        assert!(out.contains("pub fn norm(&self) -> i32 { ... }"));
        assert!(out.contains("pub struct Point {\n    pub x: i32,\n}"));
        assert!(!out.contains("a + b"));
    }

    #[test]
    fn python_bodies_become_ellipsis() {
        let src = "def double(x):\n    return x * 2\n\nclass C:\n    def m(self, y):\n        z = y + 1\n        return z\n";
        let out = compress_signatures(src, "py").expect("changed");
        assert!(out.contains("def double(x):\n    ..."));
        assert!(out.contains("def m(self, y):\n        ..."));
        assert!(!out.contains("x * 2"));
        assert!(!out.contains("y + 1"));
    }

    #[test]
    fn ts_methods_and_arrows_elide_but_expression_arrows_survive() {
        let src = "export const inc = (x: number) => x + 1;\n\nexport function big(a: number): number {\n  const b = a * 2;\n  return b;\n}\n\nclass S {\n  run(): void {\n    console.log('hi');\n  }\n}\n";
        let out = compress_signatures(src, "ts").expect("changed");
        assert!(out.contains("(x: number) => x + 1"), "expression arrow kept");
        assert!(out.contains("export function big(a: number): number { ... }"));
        assert!(out.contains("run(): void { ... }"));
        assert!(!out.contains("console.log"));
    }

    #[test]
    fn go_java_c_bodies_elide() {
        let go = "package main\n\nfunc add(a int, b int) int {\n\treturn a + b\n}\n";
        assert!(compress_signatures(go, "go")
            .expect("changed")
            .contains("func add(a int, b int) int { ... }"));

        let java = "class A {\n    int add(int a, int b) {\n        return a + b;\n    }\n}\n";
        assert!(compress_signatures(java, "java")
            .expect("changed")
            .contains("int add(int a, int b) { ... }"));

        let c = "int add(int a, int b) {\n    return a + b;\n}\n";
        assert!(compress_signatures(c, "c")
            .expect("changed")
            .contains("int add(int a, int b) { ... }"));
    }

    #[test]
    fn unsupported_extension_returns_none() {
        assert!(compress_signatures("# heading\n", "md").is_none());
        assert!(strip_comments("# heading\n", "md").is_none());
    }

    #[test]
    fn parse_errors_leave_content_untouched() {
        let src = "fn broken( {{{{\n";
        assert!(compress_signatures(src, "rs").is_none());
        assert!(strip_comments(src, "rs").is_none());
    }

    #[test]
    fn comments_stripped_without_leaving_blank_lines() {
        let src = "// file header\nfn main() {\n    // inner note\n    let x = 1; // trailing\n    /* block */ let y = 2;\n}\n";
        let out = strip_comments(src, "rs").expect("changed");
        assert!(!out.contains("file header"));
        assert!(!out.contains("inner note"));
        assert!(!out.contains("trailing"));
        assert!(!out.contains("block"));
        assert!(out.contains("let x = 1;\n"), "code after trailing comment removal intact: {out}");
        assert!(out.contains("let y = 2;"));
        assert!(!out.contains("\n\n    let x"), "whole-line comment must not leave a blank line");
        assert_eq!(out.matches("fn main()").count(), 1);
    }

    #[test]
    fn python_comments_stripped() {
        let src = "# module docstring-ish comment\nx = 1  # trailing\n\ndef f():\n    # body note\n    return x\n";
        let out = strip_comments(src, "py").expect("changed");
        assert!(!out.contains('#'));
        assert!(out.contains("x = 1\n"));
        assert!(out.contains("def f():\n    return x\n"));
    }

    #[test]
    fn strip_then_compress_compose() {
        // Order matters: `{ ... }` placeholders aren't parseable source, so
        // comment strip must run first (the merger does exactly this).
        let src = "// header\nfn work() {\n    // gone with the body\n    let a = 1;\n}\n";
        let stripped = strip_comments(src, "rs").expect("changed");
        assert!(!stripped.contains("header"));
        let compressed = compress_signatures(&stripped, "rs").expect("changed");
        assert!(compressed.contains("fn work() { ... }"));
        assert!(!compressed.contains("let a = 1"));

        // And the reverse order degrades gracefully (None, not corruption).
        let pre = compress_signatures(src, "rs").expect("changed");
        assert!(strip_comments(&pre, "rs").is_none());
    }
}
