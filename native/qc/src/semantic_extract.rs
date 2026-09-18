use std::collections::{HashMap, HashSet};
use std::path::Path;
use tree_sitter::{Language, Node, Parser};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ExtractedKind {
    Function,
    Method,
    Class,
    Interface,
    Struct,
    Enum,
    Trait,
    Type,
    Constant,
    Variable,
}

#[derive(Clone, Debug)]
pub struct ExtractedSymbol {
    pub name: String,
    pub kind: ExtractedKind,
    pub name_start_byte: usize,
    pub name_end_byte: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: usize,
    pub start_column: usize,
    pub owner_hint: Option<String>,
    pub receiver_alias: Option<String>,
    pub enclosing: Option<usize>,
    pub qualified_name: String,
}

#[derive(Clone, Debug)]
pub struct ImportBinding {
    pub local: String,
    pub imported: Option<String>,
    pub namespace: bool,
}

#[derive(Clone, Debug)]
pub struct RawImport {
    pub module: String,
    pub bindings: Vec<ImportBinding>,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug)]
pub struct RawCall {
    pub name: String,
    pub receiver: Option<String>,
    pub enclosing: Option<usize>,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RawTypeRelationKind {
    Extends,
    Implements,
}

#[derive(Clone, Debug)]
pub struct RawTypeRelation {
    pub source: usize,
    pub target: String,
    pub kind: RawTypeRelationKind,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug)]
pub struct RawReference {
    pub enclosing: Option<usize>,
    pub line: usize,
    pub column: usize,
    pub start_byte: u32,
    pub end_byte: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ExtractedFile {
    pub symbols: Vec<ExtractedSymbol>,
    pub imports: Vec<RawImport>,
    pub calls: Vec<RawCall>,
    pub type_relations: Vec<RawTypeRelation>,
    pub references: Vec<RawReference>,
    pub parse_had_error: bool,
}

impl ExtractedFile {
    pub fn logical_bytes(&self) -> usize {
        let mut bytes = std::mem::size_of::<Self>()
            .saturating_add(self.symbols.capacity().saturating_mul(std::mem::size_of::<ExtractedSymbol>()))
            .saturating_add(self.imports.capacity().saturating_mul(std::mem::size_of::<RawImport>()))
            .saturating_add(self.calls.capacity().saturating_mul(std::mem::size_of::<RawCall>()))
            .saturating_add(self.type_relations.capacity().saturating_mul(std::mem::size_of::<RawTypeRelation>()))
            .saturating_add(self.references.capacity().saturating_mul(std::mem::size_of::<RawReference>()));
        for symbol in &self.symbols {
            bytes = bytes
                .saturating_add(symbol.name.capacity())
                .saturating_add(symbol.qualified_name.capacity())
                .saturating_add(symbol.owner_hint.as_ref().map_or(0, String::capacity))
                .saturating_add(symbol.receiver_alias.as_ref().map_or(0, String::capacity));
        }
        for import in &self.imports {
            bytes = bytes
                .saturating_add(import.module.capacity())
                .saturating_add(import.bindings.capacity().saturating_mul(std::mem::size_of::<ImportBinding>()));
            for binding in &import.bindings {
                bytes = bytes
                    .saturating_add(binding.local.capacity())
                    .saturating_add(binding.imported.as_ref().map_or(0, String::capacity));
            }
        }
        for call in &self.calls {
            bytes = bytes
                .saturating_add(call.name.capacity())
                .saturating_add(call.receiver.as_ref().map_or(0, String::capacity));
        }
        for relation in &self.type_relations {
            bytes = bytes.saturating_add(relation.target.capacity());
        }
        bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SourceLanguage {
    JavaScript,
    TypeScript,
    Tsx,
    Python,
    Rust,
    Go,
}

impl SourceLanguage {
    fn from_path(path: &Path) -> Option<Self> {
        match path.extension().and_then(|v| v.to_str()).unwrap_or_default().to_ascii_lowercase().as_str() {
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" => Some(Self::TypeScript),
            "tsx" => Some(Self::Tsx),
            "py" => Some(Self::Python),
            "rs" => Some(Self::Rust),
            "go" => Some(Self::Go),
            _ => None,
        }
    }

    fn grammar(self) -> Language {
        match self {
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or_default()
}

fn field_text(node: Node<'_>, field: &str, source: &str) -> Option<String> {
    let child = node.child_by_field_name(field)?;
    let value = text(child, source).trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn identifier_name(node: Node<'_>, source: &str) -> Option<String> {
    for field in ["name", "declarator"] {
        if let Some(value) = field_text(node, field, source) {
            let value = value.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '$');
            if !value.is_empty() && !value.contains(char::is_whitespace) {
                return Some(value.to_owned());
            }
        }
    }
    None
}

fn owner_hint_from_ancestor(mut node: Node<'_>, source: &str, language: SourceLanguage) -> Option<String> {
    while let Some(parent) = node.parent() {
        match (language, parent.kind()) {
            (SourceLanguage::Rust, "impl_item") => {
                if let Some(value) = field_text(parent, "type", source) {
                    return Some(clean_type_name(&value));
                }
            }
            _ => {}
        }
        node = parent;
    }
    None
}

fn receiver_from_go_method(node: Node<'_>, source: &str) -> (Option<String>, Option<String>) {
    let Some(receiver) = node.child_by_field_name("receiver") else { return (None, None); };
    let value = text(receiver, source).trim().trim_start_matches('(').trim_end_matches(')');
    let mut parts = value.split_whitespace();
    let alias = parts.next().map(|v| v.trim_matches(|c: char| !c.is_alphanumeric() && c != '_').to_owned());
    let ty = parts.next().map(clean_type_name);
    (alias.filter(|v| !v.is_empty()), ty.filter(|v| !v.is_empty()))
}

fn clean_type_name(value: &str) -> String {
    let mut value = value.trim();
    while let Some(rest) = value.strip_prefix(['&', '*']) {
        value = rest.trim_start();
    }
    value
        .trim_matches(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .rsplit("::")
        .next()
        .unwrap_or(value)
        .rsplit('.')
        .next()
        .unwrap_or(value)
        .to_owned()
}

fn symbol_for(node: Node<'_>, source: &str, language: SourceLanguage) -> Vec<ExtractedSymbol> {
    let mut out = Vec::new();
    let kind = node.kind();
    let mut push = |name: String, symbol_kind: ExtractedKind, owner_hint: Option<String>, receiver_alias: Option<String>, span: Node<'_>| {
        let start = span.start_position();
        let name_node = span.child_by_field_name("name").or_else(|| span.child_by_field_name("declarator"));
        let (name_start_byte, name_end_byte) = name_node
            .map(|n| (n.start_byte(), n.end_byte()))
            .unwrap_or((span.start_byte(), span.start_byte().saturating_add(name.len())));
        out.push(ExtractedSymbol {
            name: name.clone(),
            kind: symbol_kind,
            name_start_byte,
            name_end_byte,
            start_byte: span.start_byte(),
            end_byte: span.end_byte(),
            start_line: start.row + 1,
            start_column: start.column + 1,
            owner_hint,
            receiver_alias,
            enclosing: None,
            qualified_name: name,
        });
    };

    match language {
        SourceLanguage::JavaScript | SourceLanguage::TypeScript | SourceLanguage::Tsx => match kind {
            "function_declaration" | "generator_function_declaration" => {
                if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Function, None, None, node); }
            }
            "class_declaration" | "abstract_class_declaration" => {
                if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Class, None, None, node); }
            }
            "interface_declaration" => {
                if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Interface, None, None, node); }
            }
            "type_alias_declaration" => {
                if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Type, None, None, node); }
            }
            "enum_declaration" => {
                if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Enum, None, None, node); }
            }
            "method_definition" | "method_signature" | "abstract_method_signature" => {
                if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Method, None, None, node); }
            }
            "variable_declarator" => {
                if let Some(name) = field_text(node, "name", source) {
                    if !name.contains(['{', '[', ',']) {
                        let value_kind = node.child_by_field_name("value").map(|n| n.kind()).unwrap_or_default();
                        let symbol_kind = if matches!(value_kind, "arrow_function" | "function_expression" | "generator_function") {
                            ExtractedKind::Function
                        } else {
                            ExtractedKind::Variable
                        };
                        push(name, symbol_kind, None, None, node);
                    }
                }
            }
            _ => {}
        },
        SourceLanguage::Python => match kind {
            "function_definition" => {
                if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Function, None, None, node); }
            }
            "class_definition" => {
                if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Class, None, None, node); }
            }
            _ => {}
        },
        SourceLanguage::Rust => match kind {
            "function_item" | "function_signature_item" => {
                if let Some(name) = identifier_name(node, source) {
                    let owner = owner_hint_from_ancestor(node, source, language);
                    let k = if owner.is_some() { ExtractedKind::Method } else { ExtractedKind::Function };
                    push(name, k, owner, None, node);
                }
            }
            "struct_item" => if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Struct, None, None, node); },
            "enum_item" => if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Enum, None, None, node); },
            "trait_item" => if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Trait, None, None, node); },
            "type_item" => if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Type, None, None, node); },
            "const_item" => if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Constant, None, None, node); },
            "static_item" => if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Variable, None, None, node); },
            _ => {}
        },
        SourceLanguage::Go => match kind {
            "function_declaration" => if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Function, None, None, node); },
            "method_declaration" => {
                if let Some(name) = identifier_name(node, source) {
                    let (alias, owner) = receiver_from_go_method(node, source);
                    push(name, ExtractedKind::Method, owner, alias, node);
                }
            }
            "type_spec" => {
                if let Some(name) = identifier_name(node, source) {
                    let ty = node.child_by_field_name("type").map(|n| n.kind()).unwrap_or_default();
                    let k = match ty { "struct_type" => ExtractedKind::Struct, "interface_type" => ExtractedKind::Interface, _ => ExtractedKind::Type };
                    push(name, k, None, None, node);
                }
            }
            "const_spec" => if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Constant, None, None, node); },
            "var_spec" => if let Some(name) = identifier_name(node, source) { push(name, ExtractedKind::Variable, None, None, node); },
            _ => {}
        },
    }
    out
}

fn type_owner(symbols: &[ExtractedSymbol], mut enclosing: Option<usize>) -> Option<usize> {
    while let Some(index) = enclosing {
        let symbol = symbols.get(index)?;
        if matches!(
            symbol.kind,
            ExtractedKind::Class
                | ExtractedKind::Interface
                | ExtractedKind::Struct
                | ExtractedKind::Trait
        ) {
            return Some(index);
        }
        enclosing = symbol.enclosing;
    }
    None
}

fn qualified_name(
    name: &str,
    owner_hint: Option<&str>,
    symbols: &[ExtractedSymbol],
    mut enclosing: Option<usize>,
) -> String {
    let mut names = vec![name.to_owned()];
    while let Some(index) = enclosing {
        let Some(symbol) = symbols.get(index) else { break; };
        if matches!(
            symbol.kind,
            ExtractedKind::Class
                | ExtractedKind::Interface
                | ExtractedKind::Struct
                | ExtractedKind::Trait
        ) {
            names.push(symbol.name.clone());
        }
        enclosing = symbol.enclosing;
    }
    if names.len() == 1 {
        if let Some(owner) = owner_hint {
            if owner != name {
                names.push(owner.to_owned());
            }
        }
    }
    names.reverse();
    names.join("::")
}

fn walk_symbols(
    node: Node<'_>,
    source: &str,
    language: SourceLanguage,
    symbols: &mut Vec<ExtractedSymbol>,
    declaration_name_spans: &mut HashSet<(usize, usize)>,
    enclosing: Option<usize>,
) {
    let mut child_enclosing = enclosing;
    for mut symbol in symbol_for(node, source, language) {
        symbol.enclosing = enclosing;
        if symbol.owner_hint.is_none() {
            if let Some(owner) = type_owner(symbols, enclosing) {
                symbol.owner_hint = Some(symbols[owner].name.clone());
                if symbol.kind == ExtractedKind::Function {
                    symbol.kind = ExtractedKind::Method;
                }
            }
        }
        symbol.qualified_name = qualified_name(
            &symbol.name,
            symbol.owner_hint.as_deref(),
            symbols,
            enclosing,
        );
        declaration_name_spans.insert((symbol.name_start_byte, symbol.name_end_byte));
        let index = symbols.len();
        symbols.push(symbol);
        child_enclosing = Some(index);
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_symbols(
            child,
            source,
            language,
            symbols,
            declaration_name_spans,
            child_enclosing,
        );
    }
}

fn strip_string_literal(value: &str) -> String {
    value.trim().trim_matches(|c| c == '\'' || c == '"' || c == '`').to_owned()
}

fn parse_js_import(node: Node<'_>, source: &str) -> Option<RawImport> {
    let raw = text(node, source).trim();
    let module = node.child_by_field_name("source").map(|n| strip_string_literal(text(n, source))).or_else(|| {
        let marker = raw.rfind(" from ")?;
        Some(strip_string_literal(raw[marker + 6..].trim().trim_end_matches(';')))
    }).or_else(|| {
        let rest = raw.strip_prefix("import ")?.trim().trim_end_matches(';');
        if rest.starts_with(['\'', '"']) { Some(strip_string_literal(rest)) } else { None }
    })?;
    let mut bindings = Vec::new();
    let before_from = raw.strip_prefix("import").unwrap_or(raw).split(" from ").next().unwrap_or_default().trim();
    if let Some(start) = before_from.find('{') {
        if let Some(end) = before_from.rfind('}') {
            for item in before_from[start + 1..end].split(',').map(str::trim).filter(|v| !v.is_empty()) {
                let item = item.strip_prefix("type ").unwrap_or(item).trim();
                let mut parts = item.split_whitespace();
                let imported = parts.next().unwrap_or_default();
                let maybe_as = parts.next();
                let alias = if maybe_as == Some("as") { parts.next().unwrap_or(imported) } else { imported };
                if !imported.is_empty() { bindings.push(ImportBinding { local: alias.to_owned(), imported: Some(imported.to_owned()), namespace: false }); }
            }
        }
    }
    if let Some(star) = before_from.find("* as ") {
        let alias = before_from[star + 5..].split(|c: char| c == ',' || c.is_whitespace()).next().unwrap_or_default();
        if !alias.is_empty() { bindings.push(ImportBinding { local: alias.to_owned(), imported: None, namespace: true }); }
    }
    let prefix = before_from.split(',').next().unwrap_or_default().trim();
    if !prefix.is_empty() && !prefix.starts_with('{') && !prefix.starts_with('*') && !prefix.starts_with(['\'', '"']) {
        let name = prefix.split_whitespace().last().unwrap_or(prefix);
        if !name.is_empty() { bindings.push(ImportBinding { local: name.to_owned(), imported: Some("default".to_owned()), namespace: false }); }
    }
    let p = node.start_position();
    Some(RawImport { module, bindings, line: p.row + 1, column: p.column + 1 })
}

fn parse_python_import(node: Node<'_>, source: &str) -> Vec<RawImport> {
    let raw = text(node, source).trim();
    let p = node.start_position();
    if let Some(rest) = raw.strip_prefix("from ") {
        let Some((module, names)) = rest.split_once(" import ") else { return Vec::new(); };
        let mut bindings = Vec::new();
        for item in names.trim_matches(['(', ')']).split(',').map(str::trim).filter(|v| !v.is_empty()) {
            let mut parts = item.split_whitespace();
            let imported = parts.next().unwrap_or_default();
            let local = if parts.next() == Some("as") { parts.next().unwrap_or(imported) } else { imported };
            bindings.push(ImportBinding { local: local.to_owned(), imported: Some(imported.to_owned()), namespace: false });
        }
        return vec![RawImport { module: module.trim().to_owned(), bindings, line: p.row + 1, column: p.column + 1 }];
    }
    let Some(rest) = raw.strip_prefix("import ") else { return Vec::new(); };
    rest.split(',').filter_map(|item| {
        let mut parts = item.split_whitespace();
        let module = parts.next()?.trim();
        let local = if parts.next() == Some("as") { parts.next().unwrap_or(module) } else { module.rsplit('.').next().unwrap_or(module) };
        Some(RawImport { module: module.to_owned(), bindings: vec![ImportBinding { local: local.to_owned(), imported: None, namespace: true }], line: p.row + 1, column: p.column + 1 })
    }).collect()
}

fn parse_rust_use(node: Node<'_>, source: &str) -> Option<RawImport> {
    let raw = text(node, source).trim().trim_end_matches(';').trim();
    let rest = raw.strip_prefix("use ")?.trim();
    let p = node.start_position();
    let mut bindings = Vec::new();
    if let Some(open) = rest.find('{') {
        let close = rest.rfind('}')?;
        let prefix = rest[..open].trim_end_matches("::");
        for item in rest[open + 1..close].split(',').map(str::trim).filter(|v| !v.is_empty()) {
            let mut parts = item.split_whitespace();
            let imported = parts.next().unwrap_or_default().trim_start_matches("self::");
            let local = if parts.next() == Some("as") { parts.next().unwrap_or(imported) } else { imported.rsplit("::").next().unwrap_or(imported) };
            if !imported.is_empty() { bindings.push(ImportBinding { local: local.to_owned(), imported: Some(imported.to_owned()), namespace: false }); }
        }
        return Some(RawImport { module: prefix.to_owned(), bindings, line: p.row + 1, column: p.column + 1 });
    }
    let mut parts = rest.split_whitespace();
    let path = parts.next()?;
    let imported = path.rsplit("::").next().unwrap_or(path);
    let local = if parts.next() == Some("as") { parts.next().unwrap_or(imported) } else { imported };
    let module = path.rsplit_once("::").map(|(m, _)| m).unwrap_or(path);
    bindings.push(ImportBinding { local: local.to_owned(), imported: Some(imported.to_owned()), namespace: false });
    Some(RawImport { module: module.to_owned(), bindings, line: p.row + 1, column: p.column + 1 })
}

fn parse_go_import(node: Node<'_>, source: &str) -> Vec<RawImport> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "import_spec" {
            let raw = text(child, source).trim();
            let Some(q) = raw.find(['\'', '"']) else { continue; };
            let alias = raw[..q].trim();
            let module = strip_string_literal(&raw[q..]);
            let local = if alias.is_empty() { module.rsplit('/').next().unwrap_or(&module).to_owned() } else { alias.to_owned() };
            let p = child.start_position();
            out.push(RawImport { module, bindings: vec![ImportBinding { local, imported: None, namespace: true }], line: p.row + 1, column: p.column + 1 });
        }
    }
    if out.is_empty() && node.kind() == "import_spec" {
        let raw = text(node, source).trim();
        if let Some(q) = raw.find(['\'', '"']) {
            let alias = raw[..q].trim();
            let module = strip_string_literal(&raw[q..]);
            let local = if alias.is_empty() { module.rsplit('/').next().unwrap_or(&module).to_owned() } else { alias.to_owned() };
            let p = node.start_position();
            out.push(RawImport { module, bindings: vec![ImportBinding { local, imported: None, namespace: true }], line: p.row + 1, column: p.column + 1 });
        }
    }
    out
}

fn callee_parts(value: &str) -> Option<(Option<String>, String)> {
    let mut value = value.trim();
    while value.starts_with('(') && value.ends_with(')') && value.len() > 2 { value = &value[1..value.len() - 1]; }
    let value = value.trim_end_matches('?');
    let mut split_at = None;
    for sep in ["::", "."] {
        if let Some(i) = value.rfind(sep) {
            if split_at.map_or(true, |(best, _): (usize, usize)| i > best) { split_at = Some((i, sep.len())); }
        }
    }
    if let Some((i, len)) = split_at {
        let receiver = value[..i].trim().trim_matches(|c: char| c == '(' || c == ')' || c == '&' || c == '*');
        let name = value[i + len..].trim().trim_matches(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'));
        if !name.is_empty() { return Some(((!receiver.is_empty()).then(|| receiver.to_owned()), name.to_owned())); }
    }
    let name = value.trim_matches(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'));
    (!name.is_empty()).then(|| (None, name.to_owned()))
}

fn descendant_identifiers(node: Node<'_>, source: &str, out: &mut Vec<String>) {
    if matches!(node.kind(), "identifier" | "type_identifier") {
        let value = text(node, source).trim();
        if !value.is_empty() { out.push(clean_type_name(value)); }
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) { descendant_identifiers(child, source, out); }
}

fn extract_type_relations(
    node: Node<'_>,
    source: &str,
    language: SourceLanguage,
    symbols: &[ExtractedSymbol],
    source_symbol: usize,
    out: &mut Vec<RawTypeRelation>,
) {
    match language {
        SourceLanguage::JavaScript | SourceLanguage::TypeScript | SourceLanguage::Tsx => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                let k = child.kind();
                let relation = if k.contains("implements") { Some(RawTypeRelationKind::Implements) } else if k.contains("heritage") || k.contains("extends") { Some(RawTypeRelationKind::Extends) } else { None };
                if let Some(kind) = relation {
                    let mut names = Vec::new(); descendant_identifiers(child, source, &mut names);
                    for target in names.into_iter().filter(|n| n != &symbols[source_symbol].name) {
                        let p = child.start_position(); out.push(RawTypeRelation { source: source_symbol, target, kind, line: p.row + 1, column: p.column + 1 });
                    }
                }
            }
        }
        SourceLanguage::Python if node.kind() == "class_definition" => {
            if let Some(superclasses) = node.child_by_field_name("superclasses") {
                let mut names = Vec::new(); descendant_identifiers(superclasses, source, &mut names);
                for target in names { let p = superclasses.start_position(); out.push(RawTypeRelation { source: source_symbol, target, kind: RawTypeRelationKind::Extends, line: p.row + 1, column: p.column + 1 }); }
            }
        }
        _ => {}
    }
}

fn rust_impl_relation(node: Node<'_>, source: &str, symbols: &[ExtractedSymbol], out: &mut Vec<RawTypeRelation>) {
    if node.kind() != "impl_item" { return; }
    let Some(trait_name) = field_text(node, "trait", source).map(|v| clean_type_name(&v)) else { return; };
    let Some(type_name) = field_text(node, "type", source).map(|v| clean_type_name(&v)) else { return; };
    let candidates: Vec<_> = symbols.iter().enumerate().filter(|(_, s)| s.name == type_name && matches!(s.kind, ExtractedKind::Struct | ExtractedKind::Enum | ExtractedKind::Type)).collect();
    if candidates.len() == 1 {
        let p = node.start_position();
        out.push(RawTypeRelation { source: candidates[0].0, target: trait_name, kind: RawTypeRelationKind::Implements, line: p.row + 1, column: p.column + 1 });
    }
}

fn is_import_node(language: SourceLanguage, kind: &str) -> bool {
    match language {
        SourceLanguage::JavaScript | SourceLanguage::TypeScript | SourceLanguage::Tsx => {
            kind == "import_statement"
        }
        SourceLanguage::Python => matches!(kind, "import_statement" | "import_from_statement"),
        SourceLanguage::Rust => kind == "use_declaration",
        SourceLanguage::Go => matches!(kind, "import_declaration" | "import_spec"),
    }
}

fn walk_facts(
    node: Node<'_>,
    source: &str,
    language: SourceLanguage,
    symbols: &[ExtractedSymbol],
    symbol_spans: &HashMap<(usize, usize), usize>,
    declaration_name_spans: &HashSet<(usize, usize)>,
    enclosing: Option<usize>,
    inside_import: bool,
    out: &mut ExtractedFile,
) {
    let own_symbol = symbol_spans
        .get(&(node.start_byte(), node.end_byte()))
        .copied();
    let current_enclosing = own_symbol.or(enclosing);
    let inside_import = inside_import || is_import_node(language, node.kind());

    match (language, node.kind()) {
        (SourceLanguage::JavaScript | SourceLanguage::TypeScript | SourceLanguage::Tsx, "import_statement") => {
            if let Some(import) = parse_js_import(node, source) { out.imports.push(import); }
        }
        (SourceLanguage::Python, "import_statement" | "import_from_statement") => out.imports.extend(parse_python_import(node, source)),
        (SourceLanguage::Rust, "use_declaration") => if let Some(import) = parse_rust_use(node, source) { out.imports.push(import); },
        (SourceLanguage::Go, "import_declaration" | "import_spec") => {
            if node.kind() == "import_declaration" || node.parent().map(|p| p.kind()) != Some("import_declaration") { out.imports.extend(parse_go_import(node, source)); }
        }
        _ => {}
    }

    let call_like = matches!(node.kind(), "call_expression" | "call" | "new_expression");
    if call_like {
        let callee = node
            .child_by_field_name("function")
            .or_else(|| node.child_by_field_name("constructor"))
            .or_else(|| node.named_child(0));
        if let Some(callee) = callee {
            if let Some((receiver, name)) = callee_parts(text(callee, source)) {
                let p = callee.start_position();
                out.calls.push(RawCall {
                    name,
                    receiver,
                    enclosing: current_enclosing,
                    line: p.row + 1,
                    column: p.column + 1,
                });
            }
        }
    }

    if let Some(source_symbol) = own_symbol {
        if matches!(
            node.kind(),
            "class_declaration"
                | "abstract_class_declaration"
                | "interface_declaration"
                | "class_definition"
        ) {
            extract_type_relations(
                node,
                source,
                language,
                symbols,
                source_symbol,
                &mut out.type_relations,
            );
        }
    }
    if language == SourceLanguage::Rust && node.kind() == "impl_item" {
        rust_impl_relation(node, source, symbols, &mut out.type_relations);
    }

    if matches!(node.kind(), "identifier" | "type_identifier")
        && !inside_import
        && !declaration_name_spans.contains(&(node.start_byte(), node.end_byte()))
    {
        let name = text(node, source).trim();
        if !name.is_empty() {
            let p = node.start_position();
            let Ok(start_byte) = u32::try_from(node.start_byte()) else { return; };
            let Ok(end_byte) = u32::try_from(node.end_byte()) else { return; };
            out.references.push(RawReference {
                enclosing: current_enclosing,
                line: p.row + 1,
                column: p.column + 1,
                start_byte,
                end_byte,
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk_facts(
            child,
            source,
            language,
            symbols,
            symbol_spans,
            declaration_name_spans,
            current_enclosing,
            inside_import,
            out,
        );
    }
}

pub fn extract(path: &Path, source: &str) -> Option<ExtractedFile> {
    let language = SourceLanguage::from_path(path)?;
    let mut parser = Parser::new();
    parser.set_language(&language.grammar()).ok()?;
    let tree = parser.parse(source, None)?;
    let mut out = ExtractedFile {
        parse_had_error: tree.root_node().has_error(),
        ..ExtractedFile::default()
    };
    let mut declaration_name_spans = HashSet::new();
    walk_symbols(
        tree.root_node(),
        source,
        language,
        &mut out.symbols,
        &mut declaration_name_spans,
        None,
    );
    let symbol_spans: HashMap<_, _> = out
        .symbols
        .iter()
        .enumerate()
        .map(|(index, symbol)| ((symbol.start_byte, symbol.end_byte), index))
        .collect();
    let symbols = std::mem::take(&mut out.symbols);
    walk_facts(
        tree.root_node(),
        source,
        language,
        &symbols,
        &symbol_spans,
        &declaration_name_spans,
        None,
        false,
        &mut out,
    );
    out.symbols = symbols;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typescript_extracts_alias_call_and_owned_method() {
        let src = r#"import { save as saveUser } from './user';
class App { run() { saveUser(); this.finish(); } finish() {} }
"#;
        let out = extract(Path::new("src/app.ts"), src).expect("extract");
        assert!(out.symbols.iter().any(|s| s.qualified_name == "App::run"));
        assert!(out.imports.iter().any(|i| i.bindings.iter().any(|b| b.local == "saveUser" && b.imported.as_deref() == Some("save"))));
        assert!(out.calls.iter().any(|c| c.name == "saveUser"));
        assert!(out.calls.iter().any(|c| c.name == "finish" && c.receiver.as_deref() == Some("this")));
    }

    #[test]
    fn python_extracts_class_methods_and_base() {
        let src = "class Base:\n    pass\nclass Child(Base):\n    def run(self):\n        self.finish()\n    def finish(self):\n        pass\n";
        let out = extract(Path::new("a.py"), src).expect("extract");
        assert!(out.symbols.iter().any(|s| s.qualified_name == "Child::run"));
        assert!(out.type_relations.iter().any(|r| r.target == "Base"));
    }
}
