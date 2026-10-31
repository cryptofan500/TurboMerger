//! Selection plans (ADR 0020): granular file selection and sub-file slicing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SelectionMode {
    #[default]
    Walk,
    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Strictness {
    #[default]
    Error,
    Warn,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SelectionPlan {
    pub version: u32,
    pub mode: SelectionMode,
    #[serde(skip_serializing_if = "is_false")]
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ordering: Option<String>,
    pub on_missing: Strictness,
    pub on_no_match: Strictness,
    #[serde(rename = "file", default)]
    pub files: Vec<FileSel>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl SelectionPlan {
    /// An explicit plan that merges exactly `paths`, in this order
    /// (`--files-from`).
    pub fn from_paths(paths: &[String]) -> Self {
        SelectionPlan {
            version: 1,
            mode: SelectionMode::Explicit,
            ordering: Some("plan".into()),
            files: paths
                .iter()
                .map(|p| FileSel {
                    path: p.clone(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    /// The plan as TOML (`--emit-selection`).
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string_pretty(self).map_err(|e| format!("cannot write selection TOML: {e}"))
    }

    /// `on_no_match = "error"`.
    pub fn strict(&self) -> bool {
        self.on_no_match == Strictness::Error
    }

    /// Parse a SelectionPlan from TOML string.
    pub fn from_toml(s: &str) -> Result<Self, String> {
        let plan: Self = toml::from_str(s).map_err(|e| format!("invalid selection TOML: {e}"))?;
        if plan.version != 1 {
            return Err(format!(
                "unsupported selection plan version: {} (expected 1)",
                plan.version
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for f in &plan.files {
            let norm = normalize_rel(&f.path).map_err(|e| format!("file '{}': {e}", f.path))?;
            if !seen.insert(norm) {
                return Err(format!("file '{}' is listed twice", f.path));
            }
            if !f.lines.is_empty() && (!f.sections.is_empty() || !f.match_lines.is_empty()) {
                return Err(format!(
                    "file '{}': `lines` cannot be combined with `sections` or `match`",
                    f.path
                ));
            }
        }
        if plan.complete {
            for f in &plan.files {
                if !f.sections.is_empty()
                    || !f.exclude_sections.is_empty()
                    || !f.lines.is_empty()
                    || !f.match_lines.is_empty()
                    || !f.sheets.is_empty()
                    || !f.exclude_sheets.is_empty()
                    || !f.columns.is_empty()
                    || !f.rows.is_empty()
                    || f.header_row.is_some()
                    || !f.sheet_overrides.is_empty()
                {
                    return Err(format!(
                        "file '{}': slice keys are forbidden when complete = true (archive mode)",
                        f.path
                    ));
                }
            }
        }
        Ok(plan)
    }

    /// Find a file selection for the given relative path.
    pub fn find_file(&self, rel_path: &str) -> Option<&FileSel> {
        let norm = normalize_rel(rel_path).ok()?;
        self.files.iter().find(|f| {
            if let Ok(p) = normalize_rel(&f.path) {
                p == norm
            } else {
                f.path == rel_path
            }
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct FileSel {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub exclude: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_sections: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preamble: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(rename = "match", skip_serializing_if = "Vec::is_empty")]
    pub match_lines: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub heading_context: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sheets: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_sheets: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_row: Option<u32>,
    #[serde(rename = "sheet", skip_serializing_if = "Vec::is_empty")]
    pub sheet_overrides: Vec<SheetSel>,
}

impl FileSel {
    pub fn to_grid_select(&self) -> GridSelect {
        GridSelect {
            sheets: self.sheets.clone(),
            exclude_sheets: self.exclude_sheets.clone(),
            columns: self.columns.clone(),
            rows: self.rows.clone(),
            header_row: self.header_row,
            sheet_overrides: self.sheet_overrides.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SheetSel {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_row: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct GridSelect {
    pub sheets: Vec<String>,
    pub exclude_sheets: Vec<String>,
    pub columns: Vec<String>,
    pub rows: Vec<String>,
    pub header_row: Option<u32>,
    pub sheet_overrides: Vec<SheetSel>,
}

/// Normalize relative path for consistent lookup:
/// replace `\` with `/`, strip `./`, reject `..` and empty paths.
pub fn normalize_rel(path: &str) -> Result<String, String> {
    let s = path.replace('\\', "/");
    let mut parts = Vec::new();
    for part in s.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(format!("relative path cannot contain '..': {path}"));
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return Err("path is empty".into());
    }
    Ok(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_selection_plan_parse() {
        let toml_str = r#"
version = 1
mode = "explicit"
ordering = "plan"
on_missing = "error"
on_no_match = "warn"

[[file]]
path = "Desktop/report.md"
label = "main report"
sections = ["own:H1:Summary*", "H2:Results"]
exclude_sections = ["H2:Appendix"]

[[file]]
path = "data/results.xlsx"
sheets = ["Sheet1"]
[[file.sheet]]
name = "Sheet1"
header_row = 4
columns = ["Name", "Score"]
"#;
        let plan = SelectionPlan::from_toml(toml_str).expect("valid plan");
        assert_eq!(plan.version, 1);
        assert_eq!(plan.mode, SelectionMode::Explicit);
        assert_eq!(plan.ordering, Some("plan".to_string()));
        assert_eq!(plan.on_missing, Strictness::Error);
        assert_eq!(plan.on_no_match, Strictness::Warn);
        assert_eq!(plan.files.len(), 2);
        assert_eq!(plan.files[0].path, "Desktop/report.md");
        assert_eq!(plan.files[0].sections.len(), 2);
        assert_eq!(plan.files[1].sheet_overrides.len(), 1);
        assert_eq!(plan.files[1].sheet_overrides[0].header_row, Some(4));
        let back = SelectionPlan::from_toml(&plan.to_toml().unwrap()).unwrap();
        assert_eq!(back, plan, "emit-selection round trip");
    }

    #[test]
    fn typos_duplicates_and_conflicts_are_refused() {
        let typo = "version = 1\n[[file]]\npath = \"a.md\"\nsection = [\"x\"]\n";
        assert!(SelectionPlan::from_toml(typo)
            .unwrap_err()
            .contains("unknown field"));
        let dup = "version = 1\n[[file]]\npath = \"a.md\"\n[[file]]\npath = \"./a.md\"\n";
        assert!(SelectionPlan::from_toml(dup).unwrap_err().contains("twice"));
        let both = "version = 1\n[[file]]\npath = \"a.md\"\nlines = [\"1-2\"]\nmatch = [\"x\"]\n";
        assert!(SelectionPlan::from_toml(both).is_err());
        let up = "version = 1\n[[file]]\npath = \"../a.md\"\n";
        assert!(SelectionPlan::from_toml(up).is_err());
    }
}
