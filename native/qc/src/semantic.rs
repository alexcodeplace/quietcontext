use crate::repomap_index::SourceRecord;
use crate::semantic_extract::{
    self, ExtractedFile, ExtractedKind, ImportBinding, RawCall, RawTypeRelationKind,
};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};

pub type NodeId = u32;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum NodeKind {
    File,
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

impl NodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Function => "function",
            Self::Method => "method",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Type => "type",
            Self::Constant => "constant",
            Self::Variable => "variable",
        }
    }

    fn callable(self) -> bool {
        matches!(self, Self::Function | Self::Method | Self::Class)
    }

    fn type_like(self) -> bool {
        matches!(self, Self::Class | Self::Interface | Self::Struct | Self::Enum | Self::Trait | Self::Type)
    }
}

impl From<ExtractedKind> for NodeKind {
    fn from(value: ExtractedKind) -> Self {
        match value {
            ExtractedKind::Function => Self::Function,
            ExtractedKind::Method => Self::Method,
            ExtractedKind::Class => Self::Class,
            ExtractedKind::Interface => Self::Interface,
            ExtractedKind::Struct => Self::Struct,
            ExtractedKind::Enum => Self::Enum,
            ExtractedKind::Trait => Self::Trait,
            ExtractedKind::Type => Self::Type,
            ExtractedKind::Constant => Self::Constant,
            ExtractedKind::Variable => Self::Variable,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EdgeKind {
    Contains,
    Imports,
    Calls,
    Extends,
    Implements,
    References,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Contains => "contains",
            Self::Imports => "imports",
            Self::Calls => "calls",
            Self::Extends => "extends",
            Self::Implements => "implements",
            Self::References => "references",
        }
    }

    fn dependency(self) -> bool {
        !matches!(self, Self::Contains)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Confidence {
    Exact,
    Scoped,
    Unique,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Scoped => "scoped",
            Self::Unique => "unique",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SemanticNode {
    pub id: NodeId,
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: String,
    pub file: String,
    pub start_line: usize,
    pub start_column: usize,
    pub end_line: usize,
    pub end_column: usize,
    pub enclosing: Option<NodeId>,
    pub owner_name: Option<String>,
    pub receiver_alias: Option<String>,
    pub evidence: String,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SemanticEdge {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub confidence: Confidence,
}

#[derive(Clone, Debug, Default)]
pub struct GraphDiagnostics {
    pub parsed_files: usize,
    pub parse_fallback_files: usize,
    pub node_count: usize,
    pub edge_count: usize,
    pub unresolved_imports: usize,
    pub unresolved_calls: usize,
    pub ambiguous_relationships: usize,
}

#[derive(Clone, Debug)]
struct ResolvedBinding {
    target_files: Vec<String>,
    imported: Option<String>,
    namespace: bool,
}

#[derive(Clone, Debug)]
struct FileFacts {
    extracted: ExtractedFile,
    local_nodes: Vec<NodeId>,
    bindings: BTreeMap<String, ResolvedBinding>,
}

#[derive(Clone, Debug)]
pub struct TraversalStep {
    pub root: NodeId,
    pub node: NodeId,
    pub via: EdgeKind,
    pub depth: usize,
    pub file: String,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug)]
pub struct PathHop {
    pub node: NodeId,
    pub via: Option<EdgeKind>,
    pub file: Option<String>,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, Default)]
pub struct SemanticGraph {
    nodes: Vec<SemanticNode>,
    edges: Vec<SemanticEdge>,
    by_name: BTreeMap<String, Vec<NodeId>>,
    by_qualified: BTreeMap<String, Vec<NodeId>>,
    by_file: BTreeMap<String, Vec<NodeId>>,
    file_nodes: BTreeMap<String, NodeId>,
    incoming: BTreeMap<NodeId, Vec<usize>>,
    outgoing: BTreeMap<NodeId, Vec<usize>>,
    diagnostics: GraphDiagnostics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Resolution {
    None,
    One(NodeId, Confidence),
    Ambiguous,
}

fn normalize_relative(path: PathBuf) -> Option<String> {
    let mut out = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(v) => out.push(v.to_string_lossy().into_owned()),
            Component::ParentDir => {
                if out.pop().is_none() { return None; }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!out.is_empty()).then(|| out.join("/"))
}

fn strip_known_extension(path: &str) -> &str {
    for ext in [".tsx", ".ts", ".jsx", ".js", ".mjs", ".cjs", ".py", ".rs", ".go"] {
        if let Some(value) = path.strip_suffix(ext) { return value; }
    }
    path
}

fn probe_file(candidates: impl IntoIterator<Item = String>, known: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    for candidate in candidates {
        if known.contains(&candidate) && !out.contains(&candidate) { out.push(candidate); }
    }
    out
}

fn js_module_files(current: &str, module: &str, known: &BTreeSet<String>) -> Vec<String> {
    if !module.starts_with('.') { return Vec::new(); }
    let parent = Path::new(current).parent().unwrap_or_else(|| Path::new(""));
    let Some(base) = normalize_relative(parent.join(module)) else { return Vec::new(); };
    let mut candidates = vec![base.clone()];
    for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs"] {
        candidates.push(format!("{base}.{ext}"));
        candidates.push(format!("{base}/index.{ext}"));
    }
    probe_file(candidates, known)
}

fn python_module_files(current: &str, module: &str, known: &BTreeSet<String>) -> Vec<String> {
    let dots = module.chars().take_while(|c| *c == '.').count();
    let tail = module[dots..].replace('.', "/");
    let mut base = Path::new(current).parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    if dots == 0 { base = PathBuf::new(); }
    else { for _ in 1..dots { base.pop(); } }
    if !tail.is_empty() { base.push(tail); }
    let Some(base) = normalize_relative(base) else { return Vec::new(); };
    probe_file([format!("{base}.py"), format!("{base}/__init__.py"), base], known)
}

fn rust_module_files(current: &str, module: &str, known: &BTreeSet<String>) -> Vec<String> {
    let mut parts: Vec<_> = module.split("::").filter(|v| !v.is_empty()).collect();
    let current_parent = Path::new(current).parent().unwrap_or_else(|| Path::new(""));
    let mut base = if parts.first() == Some(&"crate") {
        parts.remove(0);
        if current.starts_with("src/") { PathBuf::from("src") } else { PathBuf::new() }
    } else if parts.first() == Some(&"self") {
        parts.remove(0); current_parent.to_path_buf()
    } else {
        let mut p = current_parent.to_path_buf();
        while parts.first() == Some(&"super") { parts.remove(0); p.pop(); }
        p
    };
    for part in parts { base.push(part); }
    let Some(base) = normalize_relative(base) else { return Vec::new(); };
    probe_file([format!("{base}.rs"), format!("{base}/mod.rs"), base], known)
}

fn go_module_name(root: &Path) -> Option<String> {
    let source = std::fs::read_to_string(root.join("go.mod")).ok()?;
    source.lines().find_map(|line| line.trim().strip_prefix("module ").map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned))
}

fn go_module_files(module: &str, module_name: Option<&str>, known: &BTreeSet<String>) -> Vec<String> {
    let relative = if let Some(root) = module_name {
        if module == root { "" } else if let Some(rest) = module.strip_prefix(&format!("{root}/")) { rest } else { return Vec::new(); }
    } else { return Vec::new(); };
    let prefix = if relative.is_empty() { String::new() } else { format!("{relative}/") };
    known.iter().filter(|path| path.starts_with(&prefix) && path.ends_with(".go") && Path::new(path).parent().map(|p| p.to_string_lossy().replace('\\', "/") == relative).unwrap_or(relative.is_empty())).cloned().collect()
}

fn resolve_import_files(root: &Path, current: &str, module: &str, known: &BTreeSet<String>, go_module: Option<&str>) -> Vec<String> {
    match Path::new(current).extension().and_then(|v| v.to_str()).unwrap_or_default() {
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => js_module_files(current, module, known),
        "py" => python_module_files(current, module, known),
        "rs" => rust_module_files(current, module, known),
        "go" => go_module_files(module, go_module, known),
        _ => { let _ = root; Vec::new() }
    }
}

fn add_index(map: &mut BTreeMap<String, Vec<NodeId>>, key: String, id: NodeId) {
    map.entry(key).or_default().push(id);
}

impl SemanticGraph {
    pub fn build(root: &Path, files: &[SourceRecord]) -> Self {
        let known: BTreeSet<String> = files.iter().map(|f| f.relative_path.clone()).collect();
        let go_module = go_module_name(root);
        let mut graph = Self::default();
        let mut facts = BTreeMap::<String, FileFacts>::new();

        for file in files {
            let id = graph.nodes.len() as NodeId;
            graph.file_nodes.insert(file.relative_path.clone(), id);
            add_index(&mut graph.by_file, file.relative_path.clone(), id);
            graph.nodes.push(SemanticNode {
                id,
                kind: NodeKind::File,
                name: Path::new(&file.relative_path).file_name().unwrap_or_default().to_string_lossy().into_owned(),
                qualified_name: file.relative_path.clone(),
                file: file.relative_path.clone(),
                start_line: 1,
                start_column: 1,
                end_line: file.source.lines().count().max(1),
                end_column: 1,
                enclosing: None,
                owner_name: None,
                receiver_alias: None,
                evidence: String::new(),
            });
        }

        for file in files {
            let Some(extracted) = semantic_extract::extract(&file.canonical_path, &file.source) else {
                graph.diagnostics.parse_fallback_files += 1;
                continue;
            };
            graph.diagnostics.parsed_files += 1;
            if extracted.parse_had_error { graph.diagnostics.parse_fallback_files += 1; }
            let file_node = graph.file_nodes[&file.relative_path];
            let mut local_nodes = Vec::with_capacity(extracted.symbols.len());
            for symbol in &extracted.symbols {
                let id = graph.nodes.len() as NodeId;
                local_nodes.push(id);
                let enclosing = symbol.enclosing.and_then(|i| local_nodes.get(i).copied()).or(Some(file_node));
                let node = SemanticNode {
                    id,
                    kind: symbol.kind.into(),
                    name: symbol.name.clone(),
                    qualified_name: symbol.qualified_name.clone(),
                    file: file.relative_path.clone(),
                    start_line: symbol.start_line,
                    start_column: symbol.start_column,
                    end_line: symbol.end_line,
                    end_column: symbol.end_column,
                    enclosing,
                    owner_name: symbol.owner_hint.clone(),
                    receiver_alias: symbol.receiver_alias.clone(),
                    evidence: symbol.evidence.clone(),
                };
                add_index(&mut graph.by_name, node.name.clone(), id);
                add_index(&mut graph.by_qualified, node.qualified_name.clone(), id);
                add_index(&mut graph.by_file, node.file.clone(), id);
                graph.nodes.push(node);
                graph.edges.push(SemanticEdge { from: enclosing.unwrap_or(file_node), to: id, kind: EdgeKind::Contains, file: file.relative_path.clone(), line: symbol.start_line, column: symbol.start_column, confidence: Confidence::Exact });
            }
            facts.insert(file.relative_path.clone(), FileFacts { extracted, local_nodes, bindings: BTreeMap::new() });
        }

        for file in files {
            let Some(file_facts) = facts.get_mut(&file.relative_path) else { continue; };
            let file_node = graph.file_nodes[&file.relative_path];
            for import in &file_facts.extracted.imports {
                let target_files = resolve_import_files(root, &file.relative_path, &import.module, &known, go_module.as_deref());
                if target_files.is_empty() {
                    graph.diagnostics.unresolved_imports += 1;
                    continue;
                }
                for target in &target_files {
                    if let Some(&to) = graph.file_nodes.get(target) {
                        graph.edges.push(SemanticEdge { from: file_node, to, kind: EdgeKind::Imports, file: file.relative_path.clone(), line: import.line, column: import.column, confidence: Confidence::Exact });
                    }
                }
                for ImportBinding { local, imported, namespace } in &import.bindings {
                    file_facts.bindings.insert(local.clone(), ResolvedBinding { target_files: target_files.clone(), imported: imported.clone(), namespace: *namespace });
                }
            }
        }

        let file_names: Vec<String> = facts.keys().cloned().collect();
        for file_name in file_names {
            let Some(file_facts) = facts.get(&file_name).cloned() else { continue; };
            for call in &file_facts.extracted.calls {
                let from = call.enclosing.and_then(|i| file_facts.local_nodes.get(i).copied()).unwrap_or(graph.file_nodes[&file_name]);
                match graph.resolve_call(&file_name, from, call, &file_facts.bindings) {
                    Resolution::One(to, confidence) if to != from => graph.edges.push(SemanticEdge { from, to, kind: EdgeKind::Calls, file: file_name.clone(), line: call.line, column: call.column, confidence }),
                    Resolution::One(_, _) => {}
                    Resolution::Ambiguous => { graph.diagnostics.ambiguous_relationships += 1; graph.diagnostics.unresolved_calls += 1; }
                    Resolution::None => graph.diagnostics.unresolved_calls += 1,
                }
            }
            for relation in &file_facts.extracted.type_relations {
                let Some(&from) = file_facts.local_nodes.get(relation.source) else { continue; };
                match graph.resolve_type(&file_name, &relation.target, &file_facts.bindings) {
                    Resolution::One(to, confidence) if to != from => graph.edges.push(SemanticEdge { from, to, kind: match relation.kind { RawTypeRelationKind::Extends => EdgeKind::Extends, RawTypeRelationKind::Implements => EdgeKind::Implements }, file: file_name.clone(), line: relation.line, column: relation.column, confidence }),
                    Resolution::Ambiguous => graph.diagnostics.ambiguous_relationships += 1,
                    _ => {}
                }
            }
            for reference in &file_facts.extracted.references {
                let from = reference.enclosing.and_then(|i| file_facts.local_nodes.get(i).copied()).unwrap_or(graph.file_nodes[&file_name]);
                match graph.resolve_reference(&file_name, &reference.name, &file_facts.bindings) {
                    Resolution::One(to, confidence) if to != from => graph.edges.push(SemanticEdge { from, to, kind: EdgeKind::References, file: file_name.clone(), line: reference.line, column: reference.column, confidence }),
                    Resolution::Ambiguous => graph.diagnostics.ambiguous_relationships += 1,
                    _ => {}
                }
            }
        }

        graph.edges.sort();
        graph.edges.dedup();
        for (index, edge) in graph.edges.iter().enumerate() {
            graph.outgoing.entry(edge.from).or_default().push(index);
            graph.incoming.entry(edge.to).or_default().push(index);
        }
        graph.diagnostics.node_count = graph.nodes.len();
        graph.diagnostics.edge_count = graph.edges.len();
        graph
    }

    fn candidates_in_files(&self, name: &str, files: &[String], predicate: impl Fn(NodeKind) -> bool) -> Vec<NodeId> {
        let mut out = Vec::new();
        for &id in self.by_name.get(name).into_iter().flatten() {
            let node = &self.nodes[id as usize];
            if files.contains(&node.file) && predicate(node.kind) { out.push(id); }
        }
        out
    }

    fn imported_resolution(&self, local: &str, member: Option<&str>, bindings: &BTreeMap<String, ResolvedBinding>, predicate: impl Fn(NodeKind) -> bool + Copy) -> Resolution {
        let Some(binding) = bindings.get(local) else { return Resolution::None; };
        let name = if binding.namespace { member.map(str::to_owned) } else { binding.imported.clone().or_else(|| member.map(str::to_owned)) };
        let Some(name) = name else { return Resolution::None; };
        let candidates = self.candidates_in_files(&name, &binding.target_files, predicate);
        match candidates.as_slice() {
            [id] => Resolution::One(*id, Confidence::Exact),
            [] => Resolution::None,
            _ => Resolution::Ambiguous,
        }
    }

    fn same_file_resolution(&self, file: &str, name: &str, predicate: impl Fn(NodeKind) -> bool) -> Resolution {
        let candidates: Vec<_> = self.by_name.get(name).into_iter().flatten().copied().filter(|id| {
            let node = &self.nodes[*id as usize]; node.file == file && predicate(node.kind)
        }).collect();
        match candidates.as_slice() {
            [id] => Resolution::One(*id, Confidence::Scoped),
            [] => Resolution::None,
            _ => Resolution::Ambiguous,
        }
    }

    fn unique_resolution(&self, name: &str, predicate: impl Fn(NodeKind) -> bool) -> Resolution {
        let candidates: Vec<_> = self.by_name.get(name).into_iter().flatten().copied().filter(|id| predicate(self.nodes[*id as usize].kind)).collect();
        match candidates.as_slice() {
            [id] => Resolution::One(*id, Confidence::Unique),
            [] => Resolution::None,
            _ => Resolution::Ambiguous,
        }
    }

    fn owner_method(&self, owner: &str, name: &str) -> Resolution {
        let candidates: Vec<_> = self.by_name.get(name).into_iter().flatten().copied().filter(|id| {
            let node = &self.nodes[*id as usize]; node.kind == NodeKind::Method && node.owner_name.as_deref() == Some(owner)
        }).collect();
        match candidates.as_slice() {
            [id] => Resolution::One(*id, Confidence::Scoped),
            [] => Resolution::None,
            _ => Resolution::Ambiguous,
        }
    }

    fn caller_owner(&self, from: NodeId) -> Option<&str> {
        let node = self.nodes.get(from as usize)?;
        node.owner_name.as_deref().or_else(|| {
            let mut enclosing = node.enclosing;
            while let Some(id) = enclosing {
                let parent = &self.nodes[id as usize];
                if parent.kind.type_like() { return Some(parent.name.as_str()); }
                enclosing = parent.enclosing;
            }
            None
        })
    }

    fn resolve_call(&self, file: &str, from: NodeId, call: &RawCall, bindings: &BTreeMap<String, ResolvedBinding>) -> Resolution {
        let callable = |kind: NodeKind| kind.callable();
        if let Some(receiver) = call.receiver.as_deref() {
            if let Resolution::One(id, confidence) = self.imported_resolution(receiver, Some(&call.name), bindings, callable) { return Resolution::One(id, confidence); }
            if matches!(self.imported_resolution(receiver, Some(&call.name), bindings, callable), Resolution::Ambiguous) { return Resolution::Ambiguous; }
            let caller = &self.nodes[from as usize];
            let receiver_is_self = matches!(receiver, "self" | "this") || caller.receiver_alias.as_deref() == Some(receiver);
            if receiver_is_self {
                if let Some(owner) = self.caller_owner(from) {
                    let result = self.owner_method(owner, &call.name);
                    if result != Resolution::None { return result; }
                }
            }
            let receiver_type = receiver.rsplit(['.', ':']).next().unwrap_or(receiver).trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '$');
            if !receiver_type.is_empty() {
                let result = self.owner_method(receiver_type, &call.name);
                if result != Resolution::None { return result; }
            }
            return Resolution::None;
        }
        if let Resolution::One(id, confidence) = self.imported_resolution(&call.name, None, bindings, callable) { return Resolution::One(id, confidence); }
        if matches!(self.imported_resolution(&call.name, None, bindings, callable), Resolution::Ambiguous) { return Resolution::Ambiguous; }
        let same_file = self.same_file_resolution(file, &call.name, callable);
        if same_file != Resolution::None { return same_file; }
        self.unique_resolution(&call.name, callable)
    }

    fn resolve_type(&self, file: &str, name: &str, bindings: &BTreeMap<String, ResolvedBinding>) -> Resolution {
        let type_like = |kind: NodeKind| kind.type_like();
        if let Resolution::One(id, confidence) = self.imported_resolution(name, None, bindings, type_like) { return Resolution::One(id, confidence); }
        if matches!(self.imported_resolution(name, None, bindings, type_like), Resolution::Ambiguous) { return Resolution::Ambiguous; }
        let local = self.same_file_resolution(file, name, type_like);
        if local != Resolution::None { return local; }
        self.unique_resolution(name, type_like)
    }

    fn resolve_reference(&self, file: &str, name: &str, bindings: &BTreeMap<String, ResolvedBinding>) -> Resolution {
        let any_symbol = |kind: NodeKind| kind != NodeKind::File;
        if let Resolution::One(id, confidence) = self.imported_resolution(name, None, bindings, any_symbol) { return Resolution::One(id, confidence); }
        if matches!(self.imported_resolution(name, None, bindings, any_symbol), Resolution::Ambiguous) { return Resolution::Ambiguous; }
        let local = self.same_file_resolution(file, name, any_symbol);
        if local != Resolution::None { return local; }
        self.unique_resolution(name, any_symbol)
    }

    pub fn nodes(&self) -> &[SemanticNode] { &self.nodes }
    pub fn edges(&self) -> &[SemanticEdge] { &self.edges }
    pub fn diagnostics(&self) -> &GraphDiagnostics { &self.diagnostics }
    pub fn node(&self, id: NodeId) -> Option<&SemanticNode> { self.nodes.get(id as usize) }

    pub fn logical_bytes(&self) -> usize {
        let mut bytes = std::mem::size_of::<Self>();
        for node in &self.nodes {
            bytes = bytes.saturating_add(std::mem::size_of::<SemanticNode>())
                .saturating_add(node.name.len()).saturating_add(node.qualified_name.len())
                .saturating_add(node.file.len()).saturating_add(node.evidence.len())
                .saturating_add(node.owner_name.as_ref().map_or(0, String::len))
                .saturating_add(node.receiver_alias.as_ref().map_or(0, String::len));
        }
        bytes = bytes.saturating_add(self.edges.len().saturating_mul(std::mem::size_of::<SemanticEdge>()));
        for edge in &self.edges { bytes = bytes.saturating_add(edge.file.len()); }
        bytes
    }

    pub fn select_symbols(&self, query: &str, file_filter: Option<&str>) -> Vec<NodeId> {
        let mut ids = BTreeSet::new();
        for map in [&self.by_qualified, &self.by_name] {
            if let Some(found) = map.get(query) {
                for &id in found {
                    if self.nodes[id as usize].kind == NodeKind::File { continue; }
                    if let Some(filter) = file_filter {
                        let file = &self.nodes[id as usize].file;
                        if file != filter && !file.ends_with(filter) { continue; }
                    }
                    ids.insert(id);
                }
            }
        }
        ids.into_iter().collect()
    }

    pub fn select_target(&self, query: &str, file_filter: Option<&str>) -> Vec<NodeId> {
        if let Some(&id) = self.file_nodes.get(query) { return vec![id]; }
        let matching_files: Vec<_> = self.file_nodes.iter().filter(|(file, _)| file.ends_with(query)).map(|(_, id)| *id).collect();
        if matching_files.len() == 1 { return matching_files; }
        self.select_symbols(query, file_filter)
    }

    pub fn resolved_reference_edges(&self, target: NodeId) -> impl Iterator<Item = &SemanticEdge> {
        self.incoming.get(&target).into_iter().flatten().filter_map(|index| {
            let edge = &self.edges[*index];
            matches!(edge.kind, EdgeKind::References | EdgeKind::Calls).then_some(edge)
        })
    }

    fn traversal(&self, roots: &[NodeId], incoming: bool, kinds: &[EdgeKind], depth: usize, max_nodes: usize) -> Vec<TraversalStep> {
        let mut result = Vec::new();
        for &root in roots {
            let mut visited = HashSet::new();
            visited.insert(root);
            let mut queue = VecDeque::from([(root, 0usize)]);
            while let Some((current, current_depth)) = queue.pop_front() {
                if current_depth >= depth { continue; }
                let indexes = if incoming { self.incoming.get(&current) } else { self.outgoing.get(&current) };
                for &index in indexes.into_iter().flatten() {
                    let edge = &self.edges[index];
                    if !kinds.contains(&edge.kind) { continue; }
                    let next = if incoming { edge.from } else { edge.to };
                    if !visited.insert(next) { continue; }
                    let next_depth = current_depth + 1;
                    result.push(TraversalStep { root, node: next, via: edge.kind, depth: next_depth, file: edge.file.clone(), line: edge.line, column: edge.column });
                    if result.len() >= max_nodes { return result; }
                    queue.push_back((next, next_depth));
                }
            }
        }
        result
    }

    pub fn callers(&self, roots: &[NodeId], depth: usize, max_nodes: usize) -> Vec<TraversalStep> {
        self.traversal(roots, true, &[EdgeKind::Calls], depth.max(1), max_nodes)
    }

    pub fn callees(&self, roots: &[NodeId], depth: usize, max_nodes: usize) -> Vec<TraversalStep> {
        self.traversal(roots, false, &[EdgeKind::Calls], depth.max(1), max_nodes)
    }

    pub fn impact(&self, roots: &[NodeId], depth: usize, max_nodes: usize) -> Vec<TraversalStep> {
        self.traversal(roots, true, &[EdgeKind::Calls, EdgeKind::References, EdgeKind::Imports, EdgeKind::Extends, EdgeKind::Implements], depth.max(1), max_nodes)
    }

    fn expand_file_target(&self, ids: &[NodeId]) -> Vec<NodeId> {
        let mut out = BTreeSet::new();
        for &id in ids {
            out.insert(id);
            let node = &self.nodes[id as usize];
            if node.kind == NodeKind::File {
                if let Some(file_ids) = self.by_file.get(&node.file) { out.extend(file_ids.iter().copied()); }
            }
        }
        out.into_iter().collect()
    }

    pub fn dependencies(&self, roots: &[NodeId], incoming: bool, depth: usize, max_nodes: usize) -> Vec<TraversalStep> {
        let roots = self.expand_file_target(roots);
        self.traversal(&roots, incoming, &[EdgeKind::Calls, EdgeKind::References, EdgeKind::Imports, EdgeKind::Extends, EdgeKind::Implements], depth.max(1), max_nodes)
    }

    pub fn shortest_path(&self, from: &[NodeId], to: &[NodeId], max_depth: usize, max_nodes: usize) -> Option<Vec<PathHop>> {
        let from = self.expand_file_target(from);
        let targets: HashSet<NodeId> = self.expand_file_target(to).into_iter().collect();
        let mut queue = VecDeque::new();
        let mut previous = BTreeMap::<NodeId, (NodeId, usize)>::new();
        let mut roots = HashSet::new();
        for id in from { queue.push_back((id, 0usize)); roots.insert(id); }
        let mut seen: HashSet<NodeId> = roots.clone();
        let mut visited_count = 0usize;
        let found = loop {
            let Some((current, depth)) = queue.pop_front() else { break None; };
            if targets.contains(&current) { break Some(current); }
            if depth >= max_depth { continue; }
            for &index in self.outgoing.get(&current).into_iter().flatten() {
                let edge = &self.edges[index];
                if !edge.kind.dependency() { continue; }
                if !seen.insert(edge.to) { continue; }
                previous.insert(edge.to, (current, index));
                visited_count += 1;
                if visited_count >= max_nodes { break; }
                queue.push_back((edge.to, depth + 1));
            }
            if visited_count >= max_nodes { break None; }
        }?;
        let mut chain = vec![found];
        let mut cursor = found;
        while !roots.contains(&cursor) {
            let (parent, _) = *previous.get(&cursor)?;
            chain.push(parent); cursor = parent;
        }
        chain.reverse();
        let mut out = Vec::new();
        for (i, &node) in chain.iter().enumerate() {
            if i == 0 { out.push(PathHop { node, via: None, file: None, line: 0, column: 0 }); }
            else {
                let (_, index) = previous[&node]; let edge = &self.edges[index];
                out.push(PathHop { node, via: Some(edge.kind), file: Some(edge.file.clone()), line: edge.line, column: edge.column });
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repomap_index::{scan_root, ScanConfig};
    use std::fs;
    use tempfile::tempdir;

    fn graph(files: &[(&str, &str)]) -> SemanticGraph {
        let dir = tempdir().expect("temp");
        for (path, source) in files {
            let full = dir.path().join(path); fs::create_dir_all(full.parent().unwrap()).unwrap(); fs::write(full, source).unwrap();
        }
        let index = scan_root(dir.path(), &ScanConfig::default()).expect("index");
        index.semantic_graph().clone()
    }

    #[test]
    fn import_alias_resolves_call_without_cross_binding() {
        let g = graph(&[
            ("src/a.ts", "export function save() {}\n"),
            ("src/b.ts", "export function save() {}\n"),
            ("src/use.ts", "import { save as saveUser } from './a';\nexport function run(){ saveUser(); }\n"),
        ]);
        let run = g.select_symbols("run", None)[0];
        let callees = g.callees(&[run], 1, 20);
        assert_eq!(callees.len(), 1);
        assert_eq!(g.node(callees[0].node).unwrap().file, "src/a.ts");
    }

    #[test]
    fn ambiguous_bare_call_creates_no_edge() {
        let g = graph(&[
            ("a.ts", "export function save() {}\n"),
            ("b.ts", "export function save() {}\n"),
            ("c.ts", "export function run(){ save(); }\n"),
        ]);
        let run = g.select_symbols("run", None)[0];
        assert!(g.callees(&[run], 1, 20).is_empty());
        assert!(g.diagnostics().ambiguous_relationships > 0);
    }

    #[test]
    fn same_owner_method_resolution_does_not_cross_types() {
        let g = graph(&[("a.ts", "class A { run(){ this.finish(); } finish(){} }\nclass B { finish(){} }\n")]);
        let run = g.select_symbols("A::run", None)[0];
        let callees = g.callees(&[run], 1, 20);
        assert_eq!(callees.len(), 1);
        assert_eq!(g.node(callees[0].node).unwrap().qualified_name, "A::finish");
    }

    #[test]
    fn traversal_is_cycle_safe() {
        let g = graph(&[("a.ts", "export function a(){ b(); }\nfunction b(){ a(); }\n")]);
        let a = g.select_symbols("a", None)[0];
        let steps = g.callees(&[a], 10, 20);
        assert_eq!(steps.len(), 1);
        assert_eq!(g.node(steps[0].node).unwrap().name, "b");
    }

    #[test]
    fn python_import_alias_resolves_to_local_module() {
        let g = graph(&[
            ("util.py", "def save():\n    return 1\n"),
            ("app.py", "from util import save as persist\ndef run():\n    persist()\n"),
        ]);
        let run = g.select_symbols("run", None)[0];
        let steps = g.callees(&[run], 1, 20);
        assert_eq!(steps.len(), 1);
        assert_eq!(g.node(steps[0].node).unwrap().file, "util.py");
    }

    #[test]
    fn rust_use_alias_resolves_to_module_symbol() {
        let g = graph(&[
            ("src/util.rs", "pub fn save() {}\n"),
            ("src/lib.rs", "mod util;\nuse crate::util::save as persist;\npub fn run(){ persist(); }\n"),
        ]);
        let run = g.select_symbols("run", None)[0];
        let steps = g.callees(&[run], 1, 20);
        assert_eq!(steps.len(), 1);
        assert_eq!(g.node(steps[0].node).unwrap().file, "src/util.rs");
    }

    #[test]
    fn go_package_alias_resolves_to_local_module_symbol() {
        let g = graph(&[
            ("go.mod", "module example.com/qcfixture\n\ngo 1.24\n"),
            ("util/util.go", "package util\nfunc Save() {}\n"),
            ("main.go", "package main\nimport u \"example.com/qcfixture/util\"\nfunc Run(){ u.Save() }\n"),
        ]);
        let run = g.select_symbols("Run", None)[0];
        let steps = g.callees(&[run], 1, 20);
        assert_eq!(steps.len(), 1);
        assert_eq!(g.node(steps[0].node).unwrap().file, "util/util.go");
    }

    #[test]
    fn type_inheritance_is_a_semantic_edge() {
        let g = graph(&[("types.ts", "class Base {}\nclass Child extends Base {}\n")]);
        let child = g.select_symbols("Child", None)[0];
        let base = g.select_symbols("Base", None)[0];
        assert!(g.edges().iter().any(|edge| edge.from == child && edge.to == base && edge.kind == EdgeKind::Extends));
    }

    #[test]
    fn rust_trait_implementation_is_a_semantic_edge() {
        let g = graph(&[("src/lib.rs", "trait Persist {}\nstruct Store;\nimpl Persist for Store {}\n")]);
        let store = g.select_symbols("Store", None)[0];
        let persist = g.select_symbols("Persist", None)[0];
        assert!(g.edges().iter().any(|edge| edge.from == store && edge.to == persist && edge.kind == EdgeKind::Implements));
    }

    #[test]
    fn file_filter_keeps_same_name_definitions_separate() {
        let g = graph(&[("a.ts", "export function save() {}\n"), ("b.ts", "export function save() {}\n")]);
        assert_eq!(g.select_symbols("save", None).len(), 2);
        let only_a = g.select_symbols("save", Some("a.ts"));
        assert_eq!(only_a.len(), 1);
        assert_eq!(g.node(only_a[0]).unwrap().file, "a.ts");
    }
}
