//! Repo map (T3-1, aider's mechanism): tree-sitter def/ref tags → file
//! reference graph → hand-rolled PageRank → signature map rendered to a token
//! budget. The answer to "the whole repo won't fit": a ~1k-token structural
//! overview weighted toward the files everything else depends on.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::compress::{family_for_ext, parse, Family};
use crate::scanner::relative_display;
use crate::tokens;

const DAMPING: f64 = 0.85;
const PAGERANK_ITERS: usize = 30;
/// A def name claimed by more than this many files is too generic to link on.
const MAX_DEFINERS: usize = 5;
/// Cap one name's weight contribution per referencing file.
const MAX_REF_WEIGHT: usize = 5;
const MAX_DEFS_PER_FILE: usize = 40;
const MAX_SIG_CHARS: usize = 120;

/// Method-name noise that would connect everything to everything.
const NAME_STOPWORDS: &[&str] = &[
    "new",
    "from",
    "default",
    "main",
    "init",
    "get",
    "set",
    "len",
    "fmt",
    "clone",
    "drop",
    "next",
    "run",
    "build",
    "to_string",
    "into",
    "index",
    "iter",
    "test",
    "setup",
    "call",
    "close",
    "open",
    "read",
    "write",
    "update",
    "render",
    "value",
    "data",
    "self",
    "this",
];

struct FileTags {
    /// (name, one-line signature) in source order.
    defs: Vec<(String, String)>,
    /// Identifier occurrences (leaf identifier-ish tokens), with repeats.
    refs: Vec<String>,
}

/// Node kinds whose `name`-field child names a definition, per family.
fn def_kinds(family: Family) -> &'static [&'static str] {
    match family {
        Family::Rust => &[
            "function_item",
            "struct_item",
            "enum_item",
            "trait_item",
            "union_item",
            "type_item",
            "const_item",
            "static_item",
            "mod_item",
        ],
        Family::JsTs => &[
            "function_declaration",
            "generator_function_declaration",
            "class_declaration",
            "method_definition",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
            "abstract_class_declaration",
        ],
        Family::Python => &["function_definition", "class_definition"],
        Family::Go => &["function_declaration", "method_declaration", "type_spec"],
        Family::Java => &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "method_declaration",
            "constructor_declaration",
        ],
        Family::C | Family::Cpp => &[
            "struct_specifier",
            "enum_specifier",
            "union_specifier",
            "class_specifier",
        ],
    }
}

/// Unwrap C/C++ declarators (pointers, parens) down to the naming node.
fn unwrap_declarator<'a>(mut node: tree_sitter::Node<'a>) -> Option<tree_sitter::Node<'a>> {
    loop {
        match node.kind() {
            "pointer_declarator" | "parenthesized_declarator" | "reference_declarator" => {
                node = node.child_by_field_name("declarator")?;
            }
            k if k.ends_with("identifier") || k == "operator_name" || k == "destructor_name" => {
                return Some(node)
            }
            _ => return None,
        }
    }
}

/// One-line signature: def start → body start (or the def's own end),
/// whitespace-collapsed and capped.
fn signature_of(src: &str, def: tree_sitter::Node) -> String {
    let start = def.start_byte();
    let end = def
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .unwrap_or_else(|| def.end_byte())
        .max(start);
    let sig: String = src[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let sig = sig.trim_end_matches(['{', ':']).trim_end().to_string();
    if sig.chars().count() > MAX_SIG_CHARS {
        let cut: String = sig.chars().take(MAX_SIG_CHARS).collect();
        format!("{}…", cut)
    } else {
        sig
    }
}

fn extract_tags(content: &str, ext: &str) -> Option<FileTags> {
    let (family, language) = family_for_ext(ext)?;
    let tree = parse(content, &language)?;
    let root = tree.root_node();
    let kinds = def_kinds(family);

    // (start_byte, name, signature) — sorted at the end because the explicit
    // stack visits nodes in neither source nor reverse-source order.
    let mut defs: Vec<(usize, String, String)> = Vec::new();
    let mut refs: Vec<String> = Vec::new();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = node.kind();

        let name_node = if kinds.contains(&kind) {
            node.child_by_field_name("name")
        } else if matches!(family, Family::C | Family::Cpp) && kind == "function_declarator" {
            node.child_by_field_name("declarator")
                .and_then(unwrap_declarator)
        } else if family == Family::JsTs && kind == "variable_declarator" {
            // `const f = () => {}` / `const f = function () {}`
            node.child_by_field_name("value")
                .filter(|v| {
                    matches!(
                        v.kind(),
                        "arrow_function" | "function_expression" | "function"
                    )
                })
                .and_then(|_| node.child_by_field_name("name"))
        } else {
            None
        };
        if let Some(name) = name_node {
            let text = &content[name.byte_range()];
            if !text.is_empty() {
                // For function_declarator the enclosing definition carries the
                // signature; the declarator's own line is close enough.
                defs.push((
                    node.start_byte(),
                    text.to_string(),
                    signature_of(content, node),
                ));
            }
        }

        if node.named_child_count() == 0 && kind.ends_with("identifier") {
            refs.push(content[node.byte_range()].to_string());
        }
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    defs.sort_by_key(|(start, _, _)| *start);
    defs.truncate(MAX_DEFS_PER_FILE * 2); // pre-cap; render caps again
    Some(FileTags {
        defs: defs.into_iter().map(|(_, n, s)| (n, s)).collect(),
        refs,
    })
}

fn usable_name(name: &str) -> bool {
    name.chars().count() >= 3 && !NAME_STOPWORDS.contains(&name)
}

/// Build the aider-style repo map from an already-scanned file list.
/// `token_budget` bounds the rendered map (o200k tokens).
pub fn build_repo_map(root: &Path, files: &[PathBuf], token_budget: usize) -> String {
    let budget = token_budget.max(64);

    // 1. Extract tags in parallel. Files that don't parse contribute nothing.
    let tagged: Vec<(String, FileTags)> = files
        .par_iter()
        .filter_map(|path| {
            let rel = relative_display(root, path);
            let ext = Path::new(&rel)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let content = std::fs::read(path).ok()?;
            let content = String::from_utf8(content).ok()?;
            extract_tags(&content, &ext).map(|t| (rel, t))
        })
        .collect();
    if tagged.is_empty() {
        return format!(
            "Repo map: {} — no parseable source files found.\n",
            root.file_name().and_then(|n| n.to_str()).unwrap_or(".")
        );
    }

    let n = tagged.len();

    // 2. name → defining file indices.
    let mut definers: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, (_, tags)) in tagged.iter().enumerate() {
        let mut seen: HashSet<&str> = HashSet::new();
        for (name, _) in &tags.defs {
            if usable_name(name) && seen.insert(name.as_str()) {
                definers.entry(name.as_str()).or_default().push(i);
            }
        }
    }
    definers.retain(|_, v| v.len() <= MAX_DEFINERS);

    // 3. Weighted edges: ref in file i to a name defined in file j (i≠j).
    let mut out_weight = vec![0f64; n];
    // incoming[j] = (i, weight) list
    let mut incoming: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for (i, (_, tags)) in tagged.iter().enumerate() {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for r in &tags.refs {
            if definers.contains_key(r.as_str()) {
                *counts.entry(r.as_str()).or_default() += 1;
            }
        }
        for (name, count) in counts {
            let w = count.min(MAX_REF_WEIGHT) as f64;
            for &j in &definers[name] {
                if j != i {
                    incoming[j].push((i, w));
                    out_weight[i] += w;
                }
            }
        }
    }

    // 4. PageRank with uniform teleport; dangling mass spread uniformly.
    let mut rank = vec![1.0 / n as f64; n];
    for _ in 0..PAGERANK_ITERS {
        let dangling: f64 = (0..n)
            .filter(|&i| out_weight[i] == 0.0)
            .map(|i| rank[i])
            .sum();
        let base = (1.0 - DAMPING) / n as f64 + DAMPING * dangling / n as f64;
        let mut next = vec![base; n];
        for (j, sources) in incoming.iter().enumerate() {
            for &(i, w) in sources {
                next[j] += DAMPING * rank[i] * w / out_weight[i];
            }
        }
        rank = next;
    }

    // 5. Render best-first until the budget runs out.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| {
        rank[b]
            .partial_cmp(&rank[a])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| tagged[a].0.cmp(&tagged[b].0))
    });

    let header = format!(
        "Repo map: {} ({} source files; ranked by internal references, budget ~{} tokens)\n",
        root.file_name().and_then(|n| n.to_str()).unwrap_or("."),
        n,
        budget
    );
    let mut out = header;
    let mut used = tokens::count(&out);
    let mut rendered = 0usize;
    let mut over_budget = 0usize;
    for &i in order.iter() {
        let (rel, tags) = &tagged[i];
        if tags.defs.is_empty() {
            continue;
        }
        let mut chunk = format!("\n{}:\n", rel);
        for (_, sig) in tags.defs.iter().take(MAX_DEFS_PER_FILE) {
            chunk.push_str("│ ");
            chunk.push_str(sig);
            chunk.push('\n');
        }
        let chunk_tokens = tokens::count(&chunk);
        // Greedy best-first packing: an oversized file is skipped, not a
        // stopping point — smaller lower-ranked files still fit around it.
        if used + chunk_tokens > budget {
            over_budget += 1;
            continue;
        }
        out.push_str(&chunk);
        used += chunk_tokens;
        rendered += 1;
    }
    if over_budget > 0 {
        out.push_str(&format!(
            "\n… {} more files beyond the token budget.\n",
            over_budget
        ));
    }
    if rendered == 0 {
        // Budget too small for any signatures: fall back to ranked paths.
        for &i in order.iter().take(20) {
            out.push_str(&format!("{}\n", tagged[i].0));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf, Vec<PathBuf>) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("map_repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let a = root.join("src/core_widgets.rs");
        std::fs::write(
            &a,
            "pub fn widget_alpha(v: u32) -> u32 {\n    v + 1\n}\n\npub struct WidgetBeta {\n    pub size: u32,\n}\n",
        )
        .unwrap();
        let b = root.join("src/uses_alpha.rs");
        std::fs::write(
            &b,
            "use crate::core_widgets::widget_alpha;\n\npub fn helper_gamma() -> u32 {\n    widget_alpha(1) + widget_alpha(2)\n}\n",
        )
        .unwrap();
        let c = root.join("src/uses_everything.rs");
        std::fs::write(
            &c,
            "pub fn tail_delta() -> u32 {\n    let w = WidgetBeta { size: widget_alpha(3) };\n    w.size + helper_gamma()\n}\n",
        )
        .unwrap();
        let files = vec![a, b, c];
        (tmp, root, files)
    }

    #[test]
    fn rust_defs_and_refs_extracted() {
        let src = "pub fn widget_alpha(v: u32) -> u32 { helper_call(v) }\npub struct WidgetBeta { pub size: u32 }\n";
        let tags = extract_tags(src, "rs").expect("parsed");
        let names: Vec<&str> = tags.defs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["widget_alpha", "WidgetBeta"]);
        assert!(tags.defs[0]
            .1
            .contains("pub fn widget_alpha(v: u32) -> u32"));
        assert!(tags.refs.iter().any(|r| r == "helper_call"));
    }

    #[test]
    fn python_and_ts_defs_extracted() {
        let py = extract_tags("class Alpha:\n    def beta(self):\n        pass\n", "py").unwrap();
        let names: Vec<&str> = py.defs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["Alpha", "beta"]);

        let ts = extract_tags(
            "export const arrow_fn = (x: number) => x;\nexport function normal_fn(): void {}\ninterface Shape { area(): number; }\n",
            "ts",
        )
        .unwrap();
        let names: Vec<&str> = ts.defs.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"arrow_fn"));
        assert!(names.contains(&"normal_fn"));
        assert!(names.contains(&"Shape"));
    }

    #[test]
    fn most_referenced_file_ranks_first() {
        let (_tmp, root, files) = fixture();
        let map = build_repo_map(&root, &files, 4000);
        let alpha_pos = map.find("src/core_widgets.rs:").expect("core file in map");
        let gamma_pos = map.find("src/uses_alpha.rs:").expect("helper file in map");
        let delta_pos = map
            .find("src/uses_everything.rs:")
            .expect("leaf file in map");
        assert!(
            alpha_pos < gamma_pos && gamma_pos < delta_pos,
            "expected core > helper > leaf ordering:\n{}",
            map
        );
        assert!(map.contains("│ pub fn widget_alpha(v: u32) -> u32"));
        assert!(map.contains("│ pub struct WidgetBeta"));
    }

    #[test]
    fn budget_truncates_with_note() {
        let (_tmp, root, files) = fixture();
        let map = build_repo_map(&root, &files, 70);
        assert!(
            map.contains("more files beyond the token budget"),
            "expected truncation note:\n{}",
            map
        );
        assert!(
            crate::tokens::count(&map) < 200,
            "map must stay near budget"
        );
    }

    #[test]
    fn unparseable_repo_degrades_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("prose");
        std::fs::create_dir_all(&root).unwrap();
        let f = root.join("notes.md");
        std::fs::write(&f, "# just prose\n").unwrap();
        let map = build_repo_map(&root, &[f], 1000);
        assert!(map.contains("no parseable source files"));
    }
}
