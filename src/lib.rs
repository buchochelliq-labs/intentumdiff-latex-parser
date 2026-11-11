use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const LANGUAGE_ID: &str = "latex";
const ROOT_NODE_TYPE: &str = "latex_document";
const DETECT_EXTENSIONS: &[&str] = &[".tex", ".ltx"];
const DEFAULT_OLD: &str = "\\section{Old}\n";
const DEFAULT_NEW: &str = "\\section{New}\n";
const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

#[derive(Clone, Debug)]
struct EnvironmentFrame {
    id: String,
    name: String,
    line: u32,
    col: u32,
    children: Vec<SemanticNode>,
}

struct LatexParser;

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}

fn detect_language_impl(filename: &str, _content: &str) -> String {
    let lower = filename.to_lowercase();
    if DETECT_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) {
        LANGUAGE_ID.to_string()
    } else {
        String::new()
    }
}

/// Commands whose argument is projected into a typed node. The flag says whether the
/// mandatory argument is a comma-separated list (one node per item).
fn command_kind(name: &str) -> Option<(&'static str, bool)> {
    Some(match name {
        "documentclass" => ("document_class", false),
        "usepackage" | "RequirePackage" => ("package", true),
        "part" => ("part", false),
        "chapter" => ("chapter", false),
        "section" => ("section", false),
        "subsection" => ("subsection", false),
        "subsubsection" => ("subsubsection", false),
        "paragraph" => ("paragraph_heading", false),
        "label" => ("label", true),
        "ref" | "autoref" | "eqref" => ("reference", true),
        "cite" | "citep" | "citet" => ("citation", true),
        "input" | "include" => ("input", false),
        "includegraphics" => ("graphic", false),
        _ => return None,
    })
}

/// A byte range `[start, end)` within one comment-stripped source line.
type Span = (usize, usize);

/// The shape of a recognised command: `\name*[opt]...{arg}` plus trailing groups.
struct CommandShape {
    /// Byte column of a `*` modifier, if present.
    star: Option<usize>,
    options: Vec<Span>,
    arg: Span,
    after: usize,
}

/// Document under construction: top-level nodes plus the open environment stack.
struct Doc {
    root: Vec<SemanticNode>,
    stack: Vec<EnvironmentFrame>,
    /// The previous line ended right after `\begin{env}` (or one of its
    /// arguments), so `[...]`/`{...}` groups opening this line are still its
    /// arguments: LaTeX reads the line break as a space (#6).
    env_args_pending: bool,
}

impl Doc {
    fn next_id(&self) -> String {
        match self.stack.last() {
            Some(frame) => format!("{}.{}", frame.id, frame.children.len()),
            None => format!("0.{}", self.root.len()),
        }
    }

    fn push(&mut self, node: SemanticNode) {
        match self.stack.last_mut() {
            Some(frame) => frame.children.push(node),
            None => self.root.push(node),
        }
    }

    fn close_frame(&mut self, frame: EnvironmentFrame, end_line: u32, end_col: u32) {
        let hash = stable_hash("environment", &frame.name, &frame.children);
        let node = SemanticNodeBuilder::new(
            &frame.id,
            "environment",
            &frame.name,
            frame.line,
            frame.col,
            end_line,
            end_col,
            hash,
        )
        .children(frame.children)
        .build();
        self.push(node);
    }

    /// Close environments up to and including the innermost one named `expected`.
    fn close_environment(&mut self, expected: &str, end_line: u32, end_col: u32) {
        while let Some(frame) = self.stack.pop() {
            let matched = frame.name == expected;
            self.close_frame(frame, end_line, end_col);
            if matched {
                break;
            }
        }
    }

    fn close_all(&mut self, end_line: u32) {
        while let Some(frame) = self.stack.pop() {
            self.close_frame(frame, end_line, 0);
        }
    }
}

fn parse_latex(source: &str) -> String {
    let mut doc = Doc {
        root: Vec::new(),
        stack: Vec::new(),
        env_args_pending: false,
    };
    let mut total_lines = 0u32;

    for (index, raw) in source.lines().enumerate() {
        let line_no = index as u32;
        total_lines = line_no;
        let line = strip_comment(raw);
        if doc.env_args_pending && line.trim().is_empty() {
            // A comment-only line swallows its own line break; a blank line
            // is a paragraph break and ends the argument list.
            doc.env_args_pending = line.len() < raw.len();
            continue;
        }
        scan_line(&mut doc, line, line_no);
    }
    doc.close_all(total_lines);

    let root = SemanticNodeBuilder::new(
        "0",
        ROOT_NODE_TYPE,
        LANGUAGE_ID,
        0,
        0,
        total_lines,
        0,
        stable_hash(ROOT_NODE_TYPE, LANGUAGE_ID, &doc.root),
    )
    .children(doc.root)
    .build();

    match serde_json::to_string(&root) {
        Ok(serialized) => serialized,
        Err(err) => format!(r#"{{"error":"Serialisation error: {}"}}"#, err),
    }
}

/// Walk one comment-stripped line in source order. Recognised commands (and
/// `\begin`/`\end`) become typed nodes carrying their options; every other byte run
/// on the line is kept as a `text` node so no document content is discarded.
fn scan_line(doc: &mut Doc, line: &str, line_no: u32) {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut text_start = 0usize;
    if std::mem::take(&mut doc.env_args_pending) {
        i = environment_arguments(doc, line, 0, line_no);
        text_start = i;
    }
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        let name_start = i + 1;
        let mut j = name_start;
        while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
            j += 1;
        }
        if j == name_start {
            // Control symbol such as `\\`, `\{` or `\%`: ordinary text.
            i += 2;
            continue;
        }
        let name = &line[name_start..j];
        let handled = match name {
            "begin" => command_shape(line, j, false).map(|shape| {
                emit_text(doc, line, text_start, i, line_no);
                begin_environment(doc, line, i, line_no, &shape)
            }),
            "end" => command_shape(line, j, false).map(|shape| {
                emit_text(doc, line, text_start, i, line_no);
                let env_name = line[shape.arg.0..shape.arg.1].trim();
                doc.close_environment(env_name, line_no, shape.after as u32);
                shape.after
            }),
            _ => command_kind(name).and_then(|(node_type, split)| {
                let shape = command_shape(line, j, true)?;
                let values = if split {
                    split_list(line, shape.arg)
                } else {
                    trimmed(line, shape.arg).into_iter().collect()
                };
                if values.is_empty() {
                    return None;
                }
                emit_text(doc, line, text_start, i, line_no);
                for value in values {
                    emit_command_node(doc, line, line_no, node_type, value, &shape);
                }
                Some(shape.after)
            }),
        };
        match handled {
            Some(after) => {
                i = after;
                text_start = after;
            }
            None => i = j,
        }
    }
    emit_text(doc, line, text_start, bytes.len(), line_no);
}

/// Parse `*`? `[opt]`* `{arg}` after a command name ending at byte `pos`.
/// With `allow_options`, optional arguments are accepted; whitespace may precede groups.
fn command_shape(line: &str, pos: usize, allow_options: bool) -> Option<CommandShape> {
    let bytes = line.as_bytes();
    let mut j = pos;
    let star = (bytes.get(j) == Some(&b'*')).then_some(j);
    if star.is_some() {
        j += 1;
    }
    let mut options = Vec::new();
    loop {
        let k = skip_ws(bytes, j);
        if allow_options && bytes.get(k) == Some(&b'[') {
            let (inner, after) = balanced_group(bytes, k)?;
            options.push(inner);
            j = after;
        } else {
            break;
        }
    }
    let k = skip_ws(bytes, j);
    if bytes.get(k) != Some(&b'{') {
        return None;
    }
    let (arg, after) = balanced_group(bytes, k)?;
    Some(CommandShape {
        star,
        options,
        arg,
        after,
    })
}

fn begin_environment(
    doc: &mut Doc,
    line: &str,
    start: usize,
    line_no: u32,
    shape: &CommandShape,
) -> usize {
    doc.stack.push(EnvironmentFrame {
        id: doc.next_id(),
        name: line[shape.arg.0..shape.arg.1].trim().to_string(),
        line: line_no,
        col: start as u32,
        children: Vec::new(),
    });
    environment_arguments(doc, line, shape.after, line_no)
}

/// Environment arguments following `\begin{name}` from byte `pos`, e.g.
/// `{tabular}{ll}`. Spaces and tabs may separate them, and a line ending
/// after `\begin{name}` or an argument leaves the list open for the next line.
/// Returns the byte offset after the last argument.
fn environment_arguments(doc: &mut Doc, line: &str, mut pos: usize, line_no: u32) -> usize {
    let bytes = line.as_bytes();
    loop {
        let k = skip_ws(bytes, pos);
        let Some(&open) = bytes.get(k) else {
            doc.env_args_pending = true;
            break;
        };
        if open != b'[' && open != b'{' {
            break;
        }
        let Some((inner, after)) = balanced_group(bytes, k) else {
            break;
        };
        let node_type = if open == b'[' { "option" } else { "argument" };
        let (col, label) = trimmed(line, inner).unwrap_or((inner.0, ""));
        let id = doc.next_id();
        doc.push(node(&id, node_type, label, line_no, col as u32, &[]));
        pos = after;
    }
    pos
}

fn emit_command_node(
    doc: &mut Doc,
    line: &str,
    line_no: u32,
    node_type: &str,
    (col, label): (usize, &str),
    shape: &CommandShape,
) {
    let id = doc.next_id();
    let mut children = Vec::new();
    if let Some(star_col) = shape.star {
        let child_id = format!("{id}.{}", children.len());
        children.push(node(
            &child_id,
            "modifier",
            "*",
            line_no,
            star_col as u32,
            &[],
        ));
    }
    for option in &shape.options {
        for (opt_col, opt) in split_list(line, *option) {
            let child_id = format!("{id}.{}", children.len());
            children.push(node(&child_id, "option", opt, line_no, opt_col as u32, &[]));
        }
    }
    doc.push(node(&id, node_type, label, line_no, col as u32, &children));
}

fn emit_text(doc: &mut Doc, line: &str, start: usize, end: usize, line_no: u32) {
    if let Some((col, text)) = trimmed(line, (start, end)) {
        let id = doc.next_id();
        doc.push(node(&id, "text", text, line_no, col as u32, &[]));
    }
}

fn skip_ws(bytes: &[u8], mut pos: usize) -> usize {
    while bytes.get(pos).is_some_and(|b| *b == b' ' || *b == b'\t') {
        pos += 1;
    }
    pos
}

/// Balanced `{...}` or `[...]` group opening at `start`; returns the inner span and the
/// byte offset after the closing delimiter. Backslash escapes are honoured; an optional
/// `[...]` argument ends at the first `]` outside braces, as in LaTeX.
fn balanced_group(bytes: &[u8], start: usize) -> Option<(Span, usize)> {
    let open = *bytes.get(start)?;
    let close = match open {
        b'{' => b'}',
        b'[' => b']',
        _ => return None,
    };
    let mut braces = 0usize;
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                i += 2;
                continue;
            }
            b'{' => braces += 1,
            b'}' if braces > 0 => braces -= 1,
            c if c == close && braces == 0 => return Some(((start + 1, i), i + 1)),
            _ => {}
        }
        i += 1;
    }
    None
}

/// The trimmed sub-slice of `span` with its starting byte column, or `None` if blank.
fn trimmed(line: &str, (start, end): Span) -> Option<(usize, &str)> {
    let slice = &line[start..end];
    let trimmed_start = slice.trim_start();
    let text = trimmed_start.trim_end();
    if text.is_empty() {
        None
    } else {
        Some((start + slice.len() - trimmed_start.len(), text))
    }
}

/// Split a span on commas outside braces, dropping blank items.
fn split_list(line: &str, (start, end): Span) -> Vec<(usize, &str)> {
    let bytes = line.as_bytes();
    let mut items = Vec::new();
    let mut braces = 0usize;
    let mut item_start = start;
    let mut i = start;
    while i < end {
        match bytes[i] {
            b'\\' => {
                i += 2;
                continue;
            }
            b'{' => braces += 1,
            b'}' => braces = braces.saturating_sub(1),
            b',' if braces == 0 => {
                items.extend(trimmed(line, (item_start, i)));
                item_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    items.extend(trimmed(line, (item_start, end)));
    items
}

fn strip_comment(line: &str) -> &str {
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if ch == '%' && !escaped {
            return &line[..idx];
        }
        escaped = ch == '\\' && !escaped;
    }
    line
}

fn node(
    id: &str,
    node_type: &str,
    label: &str,
    line: u32,
    col: u32,
    children: &[SemanticNode],
) -> SemanticNode {
    SemanticNodeBuilder::new(
        id,
        node_type,
        label,
        line,
        col,
        line,
        col + label.len() as u32,
        stable_hash(node_type, label, children),
    )
    .children(children.to_vec())
    .build()
}

fn stable_hash(node_type: &str, label: &str, children: &[SemanticNode]) -> String {
    let mut value = format!("{node_type}:{label}");
    for child in children {
        value.push('|');
        value.push_str(&child.structural_hash);
    }
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

impl Guest for LatexParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }

    fn grammar_id() -> String {
        LANGUAGE_ID.to_string()
    }

    fn detect_language(filename: String, content: String) -> String {
        detect_language_impl(&filename, &content)
    }

    fn preprocess_source(source: String) -> String {
        source
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: DEFAULT_OLD.to_string(),
            new: DEFAULT_NEW.to_string(),
        }
    }

    fn process(input: String, _language: String, _filename: String) -> String {
        parse_latex(&input)
    }

    fn trivia_node_types() -> Vec<String> {
        vec![]
    }

    fn language_ids() -> Vec<String> {
        vec![LANGUAGE_ID.to_string()]
    }

    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }

    fn priority() -> i32 {
        0
    }
}

export!(LatexParser);

#[cfg(test)]
mod tests {
    use super::*;

    fn labels_by_type(node: &SemanticNode, node_type: &str, labels: &mut Vec<String>) {
        if node.node_type == node_type {
            labels.push(node.label.clone());
        }
        for child in &node.children {
            labels_by_type(child, node_type, labels);
        }
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert_eq!(LatexParser::get_parser_mode(), ParserMode::FullParse);
    }

    #[test]
    fn grammar_id_is_language_id() {
        assert_eq!(LatexParser::grammar_id(), LANGUAGE_ID);
        assert_eq!(LatexParser::language_ids(), vec![LANGUAGE_ID.to_string()]);
    }

    #[test]
    fn detects_latex_extensions() {
        assert_eq!(detect_language_impl("paper.tex", DEFAULT_NEW), LANGUAGE_ID);
        assert_eq!(detect_language_impl("paper.ltx", DEFAULT_NEW), LANGUAGE_ID);
    }

    #[test]
    fn process_returns_valid_json() {
        let parsed = parse_latex(DEFAULT_NEW);
        intentumdiff_plugin_sdk::testing::assert_valid_json(&parsed, LANGUAGE_ID);
        intentumdiff_plugin_sdk::testing::assert_root_node_type(
            &parsed,
            ROOT_NODE_TYPE,
            LANGUAGE_ID,
        );
    }

    #[test]
    fn process_extracts_packages_sections_refs_cites_graphics_and_environments() {
        let parsed = parse_latex(
            r#"
\documentclass[11pt]{article}
\usepackage{graphicx,hyperref}
\begin{document}
\section{Introduction}\label{sec:intro}
See \ref{sec:method} and \cite{knuth1984}.
\subsection{Figure}
\includegraphics[width=.5\textwidth]{figures/overview.png}
\input{sections/method}
\begin{equation}
E = mc^2
\end{equation}
\end{document}
"#,
        );
        let root: SemanticNode = serde_json::from_str(&parsed).unwrap();
        let mut classes = Vec::new();
        let mut packages = Vec::new();
        let mut sections = Vec::new();
        let mut subsections = Vec::new();
        let mut labels = Vec::new();
        let mut refs = Vec::new();
        let mut cites = Vec::new();
        let mut inputs = Vec::new();
        let mut graphics = Vec::new();
        let mut environments = Vec::new();
        labels_by_type(&root, "document_class", &mut classes);
        labels_by_type(&root, "package", &mut packages);
        labels_by_type(&root, "section", &mut sections);
        labels_by_type(&root, "subsection", &mut subsections);
        labels_by_type(&root, "label", &mut labels);
        labels_by_type(&root, "reference", &mut refs);
        labels_by_type(&root, "citation", &mut cites);
        labels_by_type(&root, "input", &mut inputs);
        labels_by_type(&root, "graphic", &mut graphics);
        labels_by_type(&root, "environment", &mut environments);

        assert!(classes.contains(&"article".to_string()));
        assert!(packages.contains(&"graphicx".to_string()));
        assert!(packages.contains(&"hyperref".to_string()));
        assert!(sections.contains(&"Introduction".to_string()));
        assert!(subsections.contains(&"Figure".to_string()));
        assert!(labels.contains(&"sec:intro".to_string()));
        assert!(refs.contains(&"sec:method".to_string()));
        assert!(cites.contains(&"knuth1984".to_string()));
        assert!(inputs.contains(&"sections/method".to_string()));
        assert!(graphics.contains(&"figures/overview.png".to_string()));
        assert!(environments.contains(&"document".to_string()));
        assert!(environments.contains(&"equation".to_string()));
    }

    /// (node_type, label, start_line, start_col) for every node, pre-order, below the root.
    fn flatten(source: &str) -> Vec<(String, String, u32, u32)> {
        fn walk(node: &SemanticNode, out: &mut Vec<(String, String, u32, u32)>) {
            for child in &node.children {
                out.push((
                    child.node_type.clone(),
                    child.label.clone(),
                    child.position.start_line,
                    child.position.start_col,
                ));
                walk(child, out);
            }
        }
        let root: SemanticNode = serde_json::from_str(&parse_latex(source)).unwrap();
        let mut out = Vec::new();
        walk(&root, &mut out);
        out
    }

    fn root_hash(source: &str) -> String {
        let root: SemanticNode = serde_json::from_str(&parse_latex(source)).unwrap();
        root.structural_hash
    }

    fn entry(node_type: &str, label: &str, line: u32, col: u32) -> (String, String, u32, u32) {
        (node_type.to_string(), label.to_string(), line, col)
    }

    // buchochelliq-labs/intentumdiff-latex-parser#4: class options were discarded.
    #[test]
    fn document_class_options_are_preserved() {
        let old = "\\documentclass[10pt]{article}\n\\begin{document}Hello\\end{document}\n";
        let new = "\\documentclass[12pt]{article}\n\\begin{document}Hello\\end{document}\n";
        let old_nodes = flatten(old);
        let new_nodes = flatten(new);
        assert_eq!(old_nodes[0], entry("document_class", "article", 0, 21));
        assert_eq!(old_nodes[1], entry("option", "10pt", 0, 15));
        assert_eq!(new_nodes[1], entry("option", "12pt", 0, 15));
        assert_ne!(root_hash(old), root_hash(new));
        // Adding an option to a bare class is a change too.
        let bare = flatten("\\documentclass{article}\n");
        let added = flatten("\\documentclass[12pt]{article}\n");
        assert_eq!(bare, vec![entry("document_class", "article", 0, 15)]);
        assert_eq!(
            added,
            vec![
                entry("document_class", "article", 0, 21),
                entry("option", "12pt", 0, 15),
            ]
        );
        // Multiple options are kept individually, in order.
        let multi = flatten("\\documentclass[a4paper, twocolumn]{article}\n");
        assert_eq!(multi[1], entry("option", "a4paper", 0, 15));
        assert_eq!(multi[2], entry("option", "twocolumn", 0, 24));
    }

    // buchochelliq-labs/intentumdiff-latex-parser#4: package options were discarded.
    #[test]
    fn package_options_are_preserved() {
        let old = "\\documentclass{article}\n\\usepackage[draft]{graphicx}\n\\begin{document}Hello\\end{document}\n";
        let new = "\\documentclass{article}\n\\usepackage[final]{graphicx}\n\\begin{document}Hello\\end{document}\n";
        let old_nodes = flatten(old);
        let new_nodes = flatten(new);
        assert_eq!(old_nodes[1], entry("package", "graphicx", 1, 19));
        assert_eq!(old_nodes[2], entry("option", "draft", 1, 12));
        assert_eq!(new_nodes[2], entry("option", "final", 1, 12));
        assert_ne!(root_hash(old), root_hash(new));
        // Options apply to every package in a list; braces protect inner commas.
        let nodes = flatten("\\usepackage[opt={a,b}]{x, y}\n");
        assert_eq!(
            nodes,
            vec![
                entry("package", "x", 0, 23),
                entry("option", "opt={a,b}", 0, 12),
                entry("package", "y", 0, 26),
                entry("option", "opt={a,b}", 0, 12),
            ]
        );
        // \RequirePackage is a package load as well.
        assert_eq!(
            flatten("\\RequirePackage[current]{latexrelease}\n"),
            vec![
                entry("package", "latexrelease", 0, 25),
                entry("option", "current", 0, 16),
            ]
        );
    }

    // buchochelliq-labs/intentumdiff-latex-parser#4: prose after a command was dropped.
    #[test]
    fn trailing_prose_after_command_is_preserved() {
        let old = "\\section{Title} safe\n";
        let new = "\\section{Title} danger\n";
        assert_eq!(
            flatten(old),
            vec![
                entry("section", "Title", 0, 9),
                entry("text", "safe", 0, 16)
            ]
        );
        assert_eq!(
            flatten(new),
            vec![
                entry("section", "Title", 0, 9),
                entry("text", "danger", 0, 16)
            ]
        );
        assert_ne!(root_hash(old), root_hash(new));
        // Prose between and around references keeps its order.
        assert_eq!(
            flatten("See \\ref{a} and \\cite{b,c}.\n"),
            vec![
                entry("text", "See", 0, 0),
                entry("reference", "a", 0, 9),
                entry("text", "and", 0, 12),
                entry("citation", "b", 0, 22),
                entry("citation", "c", 0, 24),
                entry("text", ".", 0, 26),
            ]
        );
    }

    // buchochelliq-labs/intentumdiff-latex-parser#4: same-line environment bodies were dropped.
    #[test]
    fn same_line_environment_body_is_preserved() {
        let old = "\\begin{document}safe\\end{document}\n";
        let new = "\\begin{document}danger\\end{document}\n";
        assert_eq!(
            flatten(old),
            vec![
                entry("environment", "document", 0, 0),
                entry("text", "safe", 0, 16)
            ]
        );
        assert_eq!(
            flatten(new),
            vec![
                entry("environment", "document", 0, 0),
                entry("text", "danger", 0, 16)
            ]
        );
        assert_ne!(root_hash(old), root_hash(new));
        // Text after \end stays outside the environment.
        assert_eq!(
            flatten("\\begin{center}x\\end{center} y\n"),
            vec![
                entry("environment", "center", 0, 0),
                entry("text", "x", 0, 14),
                entry("text", "y", 0, 28),
            ]
        );
    }

    #[test]
    fn multi_line_environment_and_arguments_are_preserved() {
        let source = "\\begin{tabular}{ll}\na & b\n\\end{tabular}\n";
        assert_eq!(
            flatten(source),
            vec![
                entry("environment", "tabular", 0, 0),
                entry("argument", "ll", 0, 16),
                entry("text", "a & b", 1, 0),
            ]
        );
        assert_ne!(
            root_hash(source),
            root_hash("\\begin{tabular}{lr}\na & b\n\\end{tabular}\n")
        );
        let root: SemanticNode = serde_json::from_str(&parse_latex(source)).unwrap();
        let env = &root.children[0];
        assert_eq!((env.position.end_line, env.position.end_col), (2, 13));
    }

    #[test]
    fn section_star_and_short_title_are_preserved() {
        assert_eq!(
            flatten("\\section*{Intro}\n"),
            vec![
                entry("section", "Intro", 0, 10),
                entry("modifier", "*", 0, 8)
            ]
        );
        assert_eq!(
            flatten("\\section[Short]{Long, title}\n"),
            vec![
                entry("section", "Long, title", 0, 16),
                entry("option", "Short", 0, 9),
            ]
        );
    }

    #[test]
    fn comments_and_layout_do_not_change_the_tree() {
        let base = "\\section{Introduction}\nHello.\n";
        assert_eq!(
            root_hash(base),
            root_hash("\\section{Introduction}% note A\nHello.\n")
        );
        assert_eq!(
            root_hash("\\section{Introduction}% note A\nHello.\n"),
            root_hash("\\section{Introduction}% note B\nHello.\n")
        );
        assert_eq!(
            root_hash(base),
            root_hash("  \\section{Introduction}  \n\n\tHello.  \n")
        );
        assert_eq!(
            root_hash(base),
            root_hash("\\section{Introduction}\n% inserted\nHello.\n")
        );
        assert_eq!(
            root_hash("\\documentclass[12pt]{article}\n"),
            root_hash("\\documentclass [ 12pt ] { article }\n")
        );
        // An escaped percent is text, not a comment.
        assert_eq!(
            flatten("50\\% off\n"),
            vec![entry("text", "50\\% off", 0, 0)]
        );
        assert_ne!(root_hash("50\\% off\n"), root_hash("50\\% on\n"));
    }

    #[test]
    fn unicode_columns_are_utf8_bytes_and_incomplete_input_is_bounded() {
        assert_eq!(
            flatten("\\section{Résumé} Café\n"),
            vec![
                entry("section", "Résumé", 0, 9),
                entry("text", "Café", 0, 19),
            ]
        );
        assert_eq!(
            flatten("\\section{Intro"),
            vec![entry("text", "\\section{Intro", 0, 0)]
        );
        assert_eq!(
            flatten("\\usepackage[draft"),
            vec![entry("text", "\\usepackage[draft", 0, 0)]
        );
        assert_eq!(
            flatten("\\begin{x}\\é\\"),
            vec![
                entry("environment", "x", 0, 0),
                entry("text", "\\é\\", 0, 9)
            ]
        );
        assert_eq!(parse_latex("\\section{A}\n"), parse_latex("\\section{A}\n"));
    }

    // buchochelliq-labs/intentumdiff-latex-parser#6: whitespace (and one line
    // break) between `\begin{env}` and its arguments is layout.
    #[test]
    fn environment_arguments_after_whitespace_are_arguments() {
        let base = "\\begin{tabular}{ll}\na & b\n\\end{tabular}\n";
        let types = |src: &str| -> Vec<(String, String)> {
            flatten(src).into_iter().map(|(t, l, _, _)| (t, l)).collect()
        };
        for variant in [
            "\\begin{tabular} {ll}\na & b\n\\end{tabular}\n",
            "\\begin{tabular}\t {ll}\na & b\n\\end{tabular}\n",
            "\\begin{tabular}\n{ll}\na & b\n\\end{tabular}\n",
            "\\begin{tabular}  \n   {ll}\na & b\n\\end{tabular}\n",
            "\\begin{tabular}% note\n% more\n  {ll}\na & b\n\\end{tabular}\n",
        ] {
            assert_eq!(types(variant), types(base), "{variant:?}");
            assert_eq!(root_hash(variant), root_hash(base), "{variant:?}");
        }
        assert_eq!(
            flatten("\\begin{tabular}\n  {ll}\n\\end{tabular}\n")[1],
            entry("argument", "ll", 1, 3)
        );
        assert_eq!(
            flatten("\\begin{tabular} [t] {ll}\n\\end{tabular}\n"),
            vec![
                entry("environment", "tabular", 0, 0),
                entry("option", "t", 0, 17),
                entry("argument", "ll", 0, 21),
            ]
        );
        assert_ne!(
            root_hash("\\begin{tabular}\n{ll}\n\\end{tabular}\n"),
            root_hash("\\begin{tabular}\n{lr}\n\\end{tabular}\n")
        );
        // A blank line ends the argument list: the group is body text.
        assert_eq!(
            types("\\begin{tabular}\n\n{ll}\n\\end{tabular}\n")[1],
            ("text".to_string(), "{ll}".to_string())
        );
    }
}
