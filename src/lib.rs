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

fn parse_latex(source: &str) -> String {
    let mut root_children = Vec::new();
    let mut stack: Vec<EnvironmentFrame> = Vec::new();
    let mut total_lines = 0u32;

    for (index, raw) in source.lines().enumerate() {
        let line_no = index as u32;
        total_lines = line_no;
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }

        if let Some(env_name) = command_arg(&line, "begin") {
            let id = next_id(stack.last(), &root_children);
            stack.push(EnvironmentFrame {
                id,
                name: env_name,
                line: line_no,
                children: Vec::new(),
            });
            continue;
        }

        if let Some(env_name) = command_arg(&line, "end") {
            close_environment(&env_name, &mut stack, &mut root_children);
            continue;
        }

        for parsed in parse_line_nodes(&line, line_no, stack.last(), &root_children) {
            if let Some(parent) = stack.last_mut() {
                parent.children.push(parsed);
            } else {
                root_children.push(parsed);
            }
        }
    }
    close_all_environments(&mut stack, &mut root_children);

    let root = SemanticNodeBuilder::new(
        "0",
        ROOT_NODE_TYPE,
        LANGUAGE_ID,
        0,
        0,
        total_lines,
        0,
        stable_hash(ROOT_NODE_TYPE, LANGUAGE_ID, &root_children),
    )
    .children(root_children)
    .build();

    match serde_json::to_string(&root) {
        Ok(serialized) => serialized,
        Err(err) => format!(r#"{{"error":"Serialisation error: {}"}}"#, err),
    }
}

fn parse_line_nodes(
    line: &str,
    line_no: u32,
    parent: Option<&EnvironmentFrame>,
    root_children: &[SemanticNode],
) -> Vec<SemanticNode> {
    let mut nodes = Vec::new();
    if let Some(class_name) = command_arg(line, "documentclass") {
        nodes.push(new_child(
            "document_class",
            &class_name,
            line,
            line_no,
            parent,
            root_children,
            &nodes,
        ));
    }
    for package in command_args(line, "usepackage") {
        nodes.push(new_child(
            "package",
            &package,
            line,
            line_no,
            parent,
            root_children,
            &nodes,
        ));
    }
    for (command, node_type) in [
        ("part", "part"),
        ("chapter", "chapter"),
        ("section", "section"),
        ("subsection", "subsection"),
        ("subsubsection", "subsubsection"),
        ("paragraph", "paragraph_heading"),
    ] {
        if let Some(title) = command_arg(line, command) {
            nodes.push(new_child(
                node_type,
                &title,
                line,
                line_no,
                parent,
                root_children,
                &nodes,
            ));
        }
    }
    for (command, node_type) in [
        ("label", "label"),
        ("ref", "reference"),
        ("autoref", "reference"),
        ("eqref", "reference"),
        ("cite", "citation"),
        ("citep", "citation"),
        ("citet", "citation"),
        ("input", "input"),
        ("include", "input"),
        ("includegraphics", "graphic"),
    ] {
        for label in command_args(line, command) {
            nodes.push(new_child(
                node_type,
                &label,
                line,
                line_no,
                parent,
                root_children,
                &nodes,
            ));
        }
    }
    if nodes.is_empty() {
        nodes.push(new_child(
            "text",
            line,
            line,
            line_no,
            parent,
            root_children,
            &nodes,
        ));
    }
    nodes
}

fn strip_comment(line: &str) -> &str {
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if ch == '%' && !escaped {
            return &line[..idx];
        }
        escaped = ch == '\\' && !escaped;
        if ch != '\\' {
            escaped = false;
        }
    }
    line
}

fn command_arg(line: &str, command: &str) -> Option<String> {
    command_args(line, command).into_iter().next()
}

fn command_args(line: &str, command: &str) -> Vec<String> {
    let mut args = Vec::new();
    let needle = format!("\\{command}");
    let mut rest = line;
    while let Some(start) = rest.find(&needle) {
        let after = &rest[start + needle.len()..];
        if after
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic())
        {
            rest = after;
            continue;
        }
        let after_options = skip_options(after.trim_start());
        let Some((arg, remaining)) = braced_argument(after_options) else {
            break;
        };
        for value in arg.split(',') {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                args.push(trimmed.to_string());
            }
        }
        rest = remaining;
    }
    args
}

fn skip_options(mut input: &str) -> &str {
    loop {
        let trimmed = input.trim_start();
        let Some(after_open) = trimmed.strip_prefix('[') else {
            return trimmed;
        };
        let Some(end) = after_open.find(']') else {
            return trimmed;
        };
        input = &after_open[end + 1..];
    }
}

fn braced_argument(input: &str) -> Option<(String, &str)> {
    let after_open = input.strip_prefix('{')?;
    let mut depth = 1usize;
    for (idx, ch) in after_open.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((after_open[..idx].trim().to_string(), &after_open[idx + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

fn next_id(parent: Option<&EnvironmentFrame>, root_children: &[SemanticNode]) -> String {
    match parent {
        Some(frame) => format!("{}.{}", frame.id, frame.children.len()),
        None => format!("0.{}", root_children.len()),
    }
}

fn new_child(
    node_type: &str,
    label: &str,
    line: &str,
    line_no: u32,
    parent: Option<&EnvironmentFrame>,
    root_children: &[SemanticNode],
    siblings: &[SemanticNode],
) -> SemanticNode {
    let parent_id = parent.map(|frame| frame.id.as_str()).unwrap_or("0");
    let base = parent
        .map(|frame| frame.children.len())
        .unwrap_or(root_children.len());
    let id = format!("{parent_id}.{}", base + siblings.len());
    let col = line.find(label).unwrap_or_default() as u32;
    node(&id, node_type, label, line_no, col, &[])
}

fn close_environment(
    expected: &str,
    stack: &mut Vec<EnvironmentFrame>,
    root_children: &mut Vec<SemanticNode>,
) {
    while let Some(frame) = stack.pop() {
        let node = node(
            &frame.id,
            "environment",
            &frame.name,
            frame.line,
            0,
            &frame.children,
        );
        let matched = frame.name == expected;
        if let Some(parent) = stack.last_mut() {
            parent.children.push(node);
        } else {
            root_children.push(node);
        }
        if matched {
            break;
        }
    }
}

fn close_all_environments(
    stack: &mut Vec<EnvironmentFrame>,
    root_children: &mut Vec<SemanticNode>,
) {
    while let Some(frame) = stack.pop() {
        let node = node(
            &frame.id,
            "environment",
            &frame.name,
            frame.line,
            0,
            &frame.children,
        );
        if let Some(parent) = stack.last_mut() {
            parent.children.push(node);
        } else {
            root_children.push(node);
        }
    }
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
        intentumdiff_plugin_sdk::testing::assert_root_node_type(&parsed, ROOT_NODE_TYPE, LANGUAGE_ID);
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
}
