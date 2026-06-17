#[cfg(test)]
mod test;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::Hash;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Parser;
use itertools::Itertools;
use log::*;
use tree_sitter::TreeCursor;

#[derive(Parser)]
#[command(name = "cargo")]
#[command(bin_name = "cargo")]
pub enum MainFlags {
    GroupImports(Flags),
}

/// Group imports in workspace source files.
///
/// This roughly corresponds to the `group_imports` unstable rustfmt option, with the difference
/// that `rustfmt` does not distinguish workspace crates from external ones.
///
/// By default, displays a diff without applying changes. Returns code 0 when no changes are
/// necessary.
/// The --fix flag allows applying the changes.
///
/// See
/// https://rust-lang.github.io/rustfmt/?version=v1.4.38&search=#group_imports
/// https://github.com/rust-lang/rustfmt/blob/master/src/reorder.rs
#[derive(Parser)]
#[clap(about, version)]
pub struct Flags {
    #[clap(default_value_os_t = std::env::current_dir().unwrap())]
    pub workspace: PathBuf,
    /// Apply changes
    #[clap(long)]
    pub fix: bool,
    #[clap(skip = true)]
    pub rustfmt: bool,
    #[clap(long, default_value_t = clap::ColorChoice::Auto)]
    color: clap::ColorChoice,
}
impl Flags {
    pub fn write_style(&self) -> env_logger::WriteStyle {
        match self.color {
            clap::ColorChoice::Auto => env_logger::WriteStyle::Auto,
            clap::ColorChoice::Always => env_logger::WriteStyle::Always,
            clap::ColorChoice::Never => env_logger::WriteStyle::Never,
        }
    }
}

#[derive(Default, Debug)]
struct Use {
    start: tree_sitter::Point,
    end: tree_sitter::Point,
    contents: String,
    module: String,
    module_decl: bool,
}
/// The variant order defines the import order within a file.
///
/// Variants are additionally collapsed into visual sections by [`UseType::section`]:
/// `std`, external crates, and "inner" code. The inner section contains, in this order,
/// workspace crates, crate-local imports (`crate::`/`super::`/`self::`) and finally module
/// declarations (`mod foo;` / re-exports). Variants that share a section are emitted
/// contiguously, with a blank line only between different sections.
#[derive(Ord, PartialOrd, Eq, PartialEq, Hash, Copy, Clone, Debug)]
enum UseType {
    Std,
    External,
    Workspace,
    Crate,
    Module,
}
impl UseType {
    /// Visual section index: `std` (0), external crates (1), inner code (2).
    fn section(self) -> u8 {
        match self {
            UseType::Std => 0,
            UseType::External => 1,
            UseType::Workspace | UseType::Crate | UseType::Module => 2,
        }
    }
}

fn node_as_utf8(node: tree_sitter::Node<'_>, source: &str) -> anyhow::Result<String> {
    Ok(node.utf8_text(source.as_bytes())?.to_string())
}
/// Process a `use` or `mod` line, extracting the module name, comments, and attributes.
fn process_line(cursor: &mut TreeCursor, source: &str) -> anyhow::Result<Use> {
    let node = cursor.node();
    let mut u = Use {
        start: node.range().start_point,
        end: node.range().end_point,
        module_decl: node.kind() == "mod_item",
        ..Default::default()
    };
    let mut contents = vec![node_as_utf8(node, source)?];
    // Include comments and cfg
    let mut sibling = node;
    while let Some(s) = sibling.prev_sibling() {
        sibling = s;

        if ["line_comment", "attribute_item", "inner_attribute_item"].contains(&sibling.kind()) {
            let content = node_as_utf8(sibling, source)?;
            // Don't take module-level comments along
            if !content.starts_with("//!") {
                u.start = sibling.range().start_point;
                contents.push(content);
            }
        } else {
            break;
        }
    }
    u.contents = contents.into_iter().rev().join("\n");
    // Find module
    cursor.goto_first_child();
    while cursor.goto_next_sibling() {
        if [
            "identifier",
            "scoped_identifier",
            "use_wildcard",
            "use_as_clause",
            "scoped_use_list",
        ]
        .contains(&cursor.node().kind())
        {
            u.module = node_as_utf8(cursor.node(), source)?
                .split("::")
                .find(|s| !s.is_empty())
                .unwrap()
                .to_string();
            break;
        }
    }
    cursor.goto_parent();
    Ok(u)
}

pub type WorkspacePackages = HashMap<String, camino::Utf8PathBuf>;

/// Returns whether the file has been changed, or would have been changed.
pub fn process_file(
    filename: &Path,
    package_name: &str,
    edition: &str,
    workspace_packages: &WorkspacePackages,
    args: &Flags,
) -> anyhow::Result<bool> {
    // Phase 1: parse with tree-sitter
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(tree_sitter_rust::language())?;
    let source = std::fs::read_to_string(filename)?;
    let tree = parser.parse(&source, None).unwrap();

    // Phase 2: find `use` and `mod` statements
    let mut uses: Vec<Use> = vec![];
    let mut mods_names = HashSet::<String>::default();
    let mut macros_defs = HashSet::<String>::default();
    let mut cursor = tree.walk();
    cursor.goto_first_child();
    loop {
        let node = cursor.node();
        if node.kind() == "macro_definition" {
            cursor.goto_first_child();
            cursor.goto_next_sibling();
            macros_defs.insert(node_as_utf8(cursor.node(), &source)?);
            cursor.goto_parent();
        }
        // Use node
        if node.kind() == "use_declaration" {
            uses.push(process_line(&mut cursor, &source)?);
        } else if node.kind() == "mod_item" {
            let mut decl_list = false;
            cursor.goto_first_child();
            loop {
                if cursor.node().kind() == "identifier" {
                    mods_names.insert(node_as_utf8(cursor.node(), &source)?);
                } else if cursor.node().kind() == "declaration_list" {
                    decl_list = true;
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
            cursor.goto_parent();
            if !decl_list {
                uses.push(process_line(&mut cursor, &source)?);
            }
            // TODO: Look into sub-modules
        } else {
            match uses.last() {
                Some(u) if node.range().start_point.row == u.end.row => {
                    // Simplification for the deletion later.
                    anyhow::bail!(
                        "use or mod expression on line {} contains another expression. This is unsupported.",
                        u.end.row
                    );
                }
                _ => {}
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    debug!("Macros: {:?}", macros_defs);
    // Special case of macros_rules declarations, where the pub use must be after the definition.
    uses.retain(|u| !macros_defs.contains(&u.module));
    debug!("Modules: {:?}", mods_names);

    // Phase 3: Group imports
    let mut grouped = BTreeMap::<UseType, Vec<&Use>>::default();
    for u in &uses {
        let import_type = if u.module == "std" {
            UseType::Std
        } else if u.module == package_name || u.module == "crate" || u.module == "super" {
            UseType::Crate
        } else if mods_names.contains(&u.module) || u.module_decl || u.module == "self" {
            UseType::Module
        } else if workspace_packages.contains_key(&u.module) {
            UseType::Workspace
        } else {
            UseType::External
        };
        grouped.entry(import_type).or_default().push(u);
    }
    debug!("Grouped uses {:#?}", grouped);

    // Phase 4: Insert into source file
    // Collapse the per-type groups into visual sections (std / external / inner). Types
    // within the same section are emitted contiguously; sections are separated by a blank
    // line.
    let mut sections: Vec<Vec<&Use>> = vec![];
    let mut current_section: Option<u8> = None;
    for (use_type, group) in &grouped {
        if current_section != Some(use_type.section()) {
            current_section = Some(use_type.section());
            sections.push(vec![]);
        }
        sections.last_mut().unwrap().extend(group.iter().copied());
    }
    let imports = sections
        .iter()
        .map(|uses| {
            uses.iter()
                .map(|u| &u.contents)
                .chain(std::iter::once(&Default::default()))
                .join("\n")
        })
        .join("\n");

    let lines: BTreeSet<usize> = grouped
        .values()
        .flatten()
        .flat_map(|l| (l.start.row..=l.end.row))
        .collect();
    let mut source_modified = source
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            if lines.iter().next() == Some(&i) {
                Some(imports.as_str())
            } else if
            // We ensured earlier that these lines do not contain anything else
            lines.contains(&i)
                ||
            // Remove previous spacing
            l.is_empty() && (i > 0 && lines.contains(&(i - 1)))
            {
                None
            } else {
                Some(l)
            }
        })
        // New line at end
        .chain(std::iter::once(""))
        .join("\n");

    // Phase 4: Run rustfmt; this should not be needed in most cases.
    // TODO: Ensure it is not needed. The difference comes from the ordering of
    // super::,crate:: etc. imports. Most of the runtime is due to running rustfmt.
    let modified = source != source_modified;
    if modified && args.rustfmt {
        // rustfmt reads from stdin, so it cannot locate the crate's Cargo.toml and would
        // otherwise default to edition 2015 (rejecting e.g. `async fn`). Pass the package's
        // edition explicitly.
        let mut cmd = std::process::Command::new("rustfmt")
            .current_dir(&args.workspace)
            .arg("--edition")
            .arg(edition)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let mut stdin = cmd.stdin.take().unwrap();
        stdin.write_all(source_modified.as_bytes())?;
        drop(stdin);
        let out = cmd.wait_with_output()?;
        anyhow::ensure!(out.status.success(), "Calling rustfmt failed");
        source_modified = String::from_utf8(out.stdout)?;
    }

    // Phase 5: Write output or diff
    let modified = source != source_modified;
    if modified {
        if !args.fix {
            warn!(
                "Diff in {:?}:\n{}",
                filename,
                prettydiff::diff_lines(&source, &source_modified).format_with_context(
                    Some(prettydiff::text::ContextConfig {
                        context_size: 5,
                        skipping_marker: "..."
                    }),
                    true
                )
            );
        } else {
            std::fs::write(filename, &source_modified)?;
            info!("Wrote {:?}", filename);
        }
    }
    Ok(modified)
}
