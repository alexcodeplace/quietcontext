use crate::repomap_index::SourceRecord;
use crate::semantic_extract::{ExtractedKind, ImportBinding, RawCall, RawTypeRelationKind};
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

    fn bare_callable(self) -> bool {
        matches!(self, Self::Function | Self::Class)
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
}

#[derive(Clone, Debug)]
pub struct SemanticNode {
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: String,
    pub file_id: NodeId,
    pub start_line: usize,
    pub enclosing: Option<NodeId>,
    pub owner_name: Option<String>,
    pub receiver_alias: Option<String>,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SemanticEdge {
    pub from: NodeId,
    pub to: NodeId,
    pub kind: EdgeKind,
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
    target_files: Vec<NodeId>,
    imported: Option<String>,
    namespace: bool,
}

#[derive(Clone, Debug)]
struct FileResolutionState {
    local_nodes: Vec<NodeId>,
    bindings: BTreeMap<String, ResolvedBinding>,
}

#[derive(Clone, Debug)]
pub struct TraversalStep {
    pub root: NodeId,
    pub node: NodeId,
    pub via: EdgeKind,
    pub depth: usize,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug)]
pub struct PathHop {
    pub node: NodeId,
    pub via: Option<EdgeKind>,
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

fn probe_file(candidates: impl IntoIterator<Item = String>, known: &BTreeSet<&str>) -> Vec<String> {
    let mut out = Vec::new();
    for candidate in candidates {
        if known.contains(candidate.as_str()) && !out.contains(&candidate) { out.push(candidate); }
    }
    out
}

fn js_module_files(current: &str, module: &str, known: &BTreeSet<&str>) -> Vec<String> {
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

fn python_module_files(current: &str, module: &str, known: &BTreeSet<&str>) -> Vec<String> {
    let dots = module.chars().take_while(|c| *c == '.').count();
    let tail = module[dots..].replace('.', "/");
    let mut base = Path::new(current).parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    if dots == 0 { base = PathBuf::new(); }
    else { for _ in 1..dots { base.pop(); } }
    if !tail.is_empty() { base.push(tail); }
    let Some(base) = normalize_relative(base) else { return Vec::new(); };
    probe_file([format!("{base}.py"), format!("{base}/__init__.py"), base], known)
}

fn rust_module_files(current: &str, module: &str, known: &BTreeSet<&str>) -> Vec<String> {
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

fn go_module_files(module: &str, module_name: Option<&str>, known: &BTreeSet<&str>) -> Vec<String> {
    let relative = if let Some(root) = module_name {
        if module == root { "" } else if let Some(rest) = module.strip_prefix(&format!("{root}/")) { rest } else { return Vec::new(); }
    } else { return Vec::new(); };
    let prefix = if relative.is_empty() { String::new() } else { format!("{relative}/") };
    known.iter().filter(|path| path.starts_with(&prefix) && path.ends_with(".go") && Path::new(path).parent().map(|p| p.to_string_lossy().replace('\\', "/") == relative).unwrap_or(relative.is_empty())).map(|path| (*path).to_owned()).collect()
}

fn resolve_import_files(root: &Path, current: &str, module: &str, known: &BTreeSet<&str>, go_module: Option<&str>) -> Vec<String> {
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
        let known: BTreeSet<&str> = files.iter().map(|f| f.relative_path.as_str()).collect();
        let go_module = go_module_name(root);
        let mut graph = Self::default();
        let mut states = BTreeMap::<&str, FileResolutionState>::new();

        for file in files {
            let id = graph.nodes.len() as NodeId;
            graph.file_nodes.insert(file.relative_path.clone(), id);
            add_index(&mut graph.by_file, file.relative_path.clone(), id);
            graph.nodes.push(SemanticNode {
                kind: NodeKind::File,
                name: Path::new(&file.relative_path).file_name().unwrap_or_default().to_string_lossy().into_owned(),
                qualified_name: file.relative_path.clone(),
                file_id: id,
                start_line: 1,
                enclosing: None,
                owner_name: None,
                receiver_alias: None,
            });
        }

        for file in files {
            let Some(extracted) = file.semantic_facts() else {
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
                    kind: symbol.kind.into(),
                    name: symbol.name.clone(),
                    qualified_name: symbol.qualified_name.clone(),
                    file_id: file_node,
                    start_line: symbol.start_line,
                    enclosing,
                    owner_name: symbol.owner_hint.clone(),
                    receiver_alias: symbol.receiver_alias.clone(),
                };
                add_index(&mut graph.by_name, node.name.clone(), id);
                add_index(&mut graph.by_qualified, node.qualified_name.clone(), id);
                add_index(&mut graph.by_file, file.relative_path.clone(), id);
                graph.nodes.push(node);
                graph.edges.push(SemanticEdge { from: enclosing.unwrap_or(file_node), to: id, kind: EdgeKind::Contains, line: symbol.start_line, column: symbol.start_column, confidence: Confidence::Exact });
            }
            states.insert(file.relative_path.as_str(), FileResolutionState {
                local_nodes,
                bindings: BTreeMap::new(),
            });
        }

        for file in files {
            let Some(file_state) = states.get_mut(file.relative_path.as_str()) else { continue; };
            let Some(extracted) = file.semantic_facts() else { continue; };
            let file_node = graph.file_nodes[&file.relative_path];
            for import in &extracted.imports {
                let target_paths = resolve_import_files(root, &file.relative_path, &import.module, &known, go_module.as_deref());
                let target_files: Vec<NodeId> = target_paths
                    .iter()
                    .filter_map(|target| graph.file_nodes.get(target).copied())
                    .collect();
                if target_files.is_empty() {
                    graph.diagnostics.unresolved_imports += 1;
                    continue;
                }
                for &to in &target_files {
                    graph.edges.push(SemanticEdge { from: file_node, to, kind: EdgeKind::Imports, line: import.line, column: import.column, confidence: Confidence::Exact });
                }
                for ImportBinding { local, imported, namespace } in &import.bindings {
                    file_state.bindings.insert(local.clone(), ResolvedBinding { target_files: target_files.clone(), imported: imported.clone(), namespace: *namespace });
                }
            }
        }

        for file in files {
            let file_name = file.relative_path.as_str();
            let Some(file_state) = states.get(file_name) else { continue; };
            let Some(extracted) = file.semantic_facts() else { continue; };
            for call in &extracted.calls {
                let from = call.enclosing.and_then(|i| file_state.local_nodes.get(i).copied()).unwrap_or(graph.file_nodes[file_name]);
                match graph.resolve_call(file_name, from, call, &file_state.bindings) {
                    Resolution::One(to, confidence) if to != from => graph.edges.push(SemanticEdge { from, to, kind: EdgeKind::Calls, line: call.line, column: call.column, confidence }),
                    Resolution::One(_, _) => {}
                    Resolution::Ambiguous => { graph.diagnostics.ambiguous_relationships += 1; graph.diagnostics.unresolved_calls += 1; }
                    Resolution::None => graph.diagnostics.unresolved_calls += 1,
                }
            }
            for relation in &extracted.type_relations {
                let Some(&from) = file_state.local_nodes.get(relation.source) else { continue; };
                match graph.resolve_type(file_name, from, &relation.target, &file_state.bindings) {
                    Resolution::One(to, confidence) if to != from => graph.edges.push(SemanticEdge { from, to, kind: match relation.kind { RawTypeRelationKind::Extends => EdgeKind::Extends, RawTypeRelationKind::Implements => EdgeKind::Implements }, line: relation.line, column: relation.column, confidence }),
                    Resolution::Ambiguous => graph.diagnostics.ambiguous_relationships += 1,
                    _ => {}
                }
            }
            for reference in &extracted.references {
                let from = reference.enclosing.and_then(|i| file_state.local_nodes.get(i).copied()).unwrap_or(graph.file_nodes[file_name]);
                let start = reference.start_byte as usize;
                let end = reference.end_byte as usize;
                let name = file.source.get(start..end).unwrap_or_default().trim();
                if name.is_empty() { continue; }
                match graph.resolve_reference(file_name, from, name, &file_state.bindings) {
                    Resolution::One(to, confidence) if to != from => graph.edges.push(SemanticEdge { from, to, kind: EdgeKind::References, line: reference.line, column: reference.column, confidence }),
                    Resolution::Ambiguous => graph.diagnostics.ambiguous_relationships += 1,
                    _ => {}
                }
            }
        }

        graph.edges.sort();
        graph.edges.dedup();
        for (index, edge) in graph.edges.iter().enumerate() {
            if edge.kind == EdgeKind::Contains { continue; }
            graph.outgoing.entry(edge.from).or_default().push(index);
            graph.incoming.entry(edge.to).or_default().push(index);
        }
        graph.diagnostics.node_count = graph.nodes.len();
        graph.diagnostics.edge_count = graph.edges.len();
        graph
    }

    fn is_top_level(&self, id: NodeId) -> bool {
        let Some(enclosing) = self.nodes[id as usize].enclosing else { return false; };
        self.nodes[enclosing as usize].kind == NodeKind::File
    }

    fn visible_from(&self, candidate: NodeId, from: NodeId) -> bool {
        let Some(candidate_scope) = self.nodes[candidate as usize].enclosing else {
            return false;
        };
        if self.nodes[candidate_scope as usize].kind == NodeKind::File {
            return true;
        }
        let mut scope = Some(from);
        while let Some(id) = scope {
            if id == candidate_scope {
                return true;
            }
            scope = self.nodes[id as usize].enclosing;
        }
        false
    }

    fn resolution_from_ids(
        &self,
        ids: impl Iterator<Item = NodeId>,
        confidence: Confidence,
        predicate: impl Fn(NodeId, &SemanticNode) -> bool,
    ) -> Resolution {
        let mut found = None;
        for id in ids {
            let node = &self.nodes[id as usize];
            if !predicate(id, node) {
                continue;
            }
            if found.is_some() {
                return Resolution::Ambiguous;
            }
            found = Some(id);
        }
        found.map_or(Resolution::None, |id| Resolution::One(id, confidence))
    }

    fn imported_resolution(
        &self,
        local: &str,
        member: Option<&str>,
        bindings: &BTreeMap<String, ResolvedBinding>,
        predicate: impl Fn(NodeKind) -> bool + Copy,
    ) -> Resolution {
        let Some(binding) = bindings.get(local) else {
            return Resolution::None;
        };
        let name = if binding.namespace {
            member
        } else {
            binding.imported.as_deref().or(member)
        };
        let Some(name) = name else {
            return Resolution::None;
        };
        self.resolution_from_ids(
            self.by_name
                .get(name)
                .into_iter()
                .flatten()
                .copied(),
            Confidence::Exact,
            |id, node| {
                binding.target_files.contains(&node.file_id)
                    && self.is_top_level(id)
                    && predicate(node.kind)
            },
        )
    }

    fn same_file_resolution(
        &self,
        file: &str,
        from: NodeId,
        name: &str,
        predicate: impl Fn(NodeKind) -> bool,
    ) -> Resolution {
        self.resolution_from_ids(
            self.by_name
                .get(name)
                .into_iter()
                .flatten()
                .copied(),
            Confidence::Scoped,
            |id, node| self.file_path(id) == Some(file) && self.visible_from(id, from) && predicate(node.kind),
        )
    }

    fn package_resolution(
        &self,
        file: &str,
        name: &str,
        predicate: impl Fn(NodeKind) -> bool,
    ) -> Resolution {
        if Path::new(file).extension().and_then(|value| value.to_str()) != Some("go") {
            return Resolution::None;
        }
        let package_dir = Path::new(file).parent();
        self.resolution_from_ids(
            self.by_name
                .get(name)
                .into_iter()
                .flatten()
                .copied(),
            Confidence::Scoped,
            |id, node| {
                self.is_top_level(id)
                    && self.file_path(id).and_then(|candidate| Path::new(candidate).parent()) == package_dir
                    && predicate(node.kind)
            },
        )
    }

    fn owner_method(&self, owner: &str, name: &str) -> Resolution {
        self.resolution_from_ids(
            self.by_name
                .get(name)
                .into_iter()
                .flatten()
                .copied(),
            Confidence::Scoped,
            |_id, node| node.kind == NodeKind::Method && node.owner_name.as_deref() == Some(owner),
        )
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

    fn resolve_call(
        &self,
        file: &str,
        from: NodeId,
        call: &RawCall,
        bindings: &BTreeMap<String, ResolvedBinding>,
    ) -> Resolution {
        let callable = |kind: NodeKind| kind.bare_callable();
        if let Some(receiver) = call.receiver.as_deref() {
            match self.imported_resolution(receiver, Some(&call.name), bindings, callable) {
                Resolution::One(id, confidence) => return Resolution::One(id, confidence),
                Resolution::Ambiguous => return Resolution::Ambiguous,
                Resolution::None => {}
            }
            let caller = &self.nodes[from as usize];
            let receiver_is_self = matches!(receiver, "self" | "this")
                || caller.receiver_alias.as_deref() == Some(receiver);
            if receiver_is_self {
                if let Some(owner) = self.caller_owner(from) {
                    let result = self.owner_method(owner, &call.name);
                    if result != Resolution::None {
                        return result;
                    }
                }
            }
            let receiver_type = receiver
                .rsplit(['.', ':'])
                .next()
                .unwrap_or(receiver)
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '$');
            if !receiver_type.is_empty() {
                let result = self.owner_method(receiver_type, &call.name);
                if result != Resolution::None {
                    return result;
                }
            }
            return Resolution::None;
        }
        match self.imported_resolution(&call.name, None, bindings, callable) {
            Resolution::One(id, confidence) => return Resolution::One(id, confidence),
            Resolution::Ambiguous => return Resolution::Ambiguous,
            Resolution::None => {}
        }
        let same_file = self.same_file_resolution(file, from, &call.name, callable);
        if same_file != Resolution::None {
            return same_file;
        }
        self.package_resolution(file, &call.name, callable)
    }

    fn resolve_type(
        &self,
        file: &str,
        from: NodeId,
        name: &str,
        bindings: &BTreeMap<String, ResolvedBinding>,
    ) -> Resolution {
        let type_like = |kind: NodeKind| kind.type_like();
        match self.imported_resolution(name, None, bindings, type_like) {
            Resolution::One(id, confidence) => return Resolution::One(id, confidence),
            Resolution::Ambiguous => return Resolution::Ambiguous,
            Resolution::None => {}
        }
        let local = self.same_file_resolution(file, from, name, type_like);
        if local != Resolution::None {
            return local;
        }
        self.package_resolution(file, name, type_like)
    }

    fn resolve_reference(
        &self,
        file: &str,
        from: NodeId,
        name: &str,
        bindings: &BTreeMap<String, ResolvedBinding>,
    ) -> Resolution {
        let any_symbol = |kind: NodeKind| kind != NodeKind::File;
        match self.imported_resolution(name, None, bindings, any_symbol) {
            Resolution::One(id, confidence) => return Resolution::One(id, confidence),
            Resolution::Ambiguous => return Resolution::Ambiguous,
            Resolution::None => {}
        }
        let local = self.same_file_resolution(file, from, name, any_symbol);
        if local != Resolution::None {
            return local;
        }
        self.package_resolution(file, name, any_symbol)
    }

    #[cfg(test)]
    pub fn edges(&self) -> &[SemanticEdge] { &self.edges }
    #[cfg(test)]
    pub fn diagnostics(&self) -> &GraphDiagnostics { &self.diagnostics }
    pub fn node(&self, id: NodeId) -> Option<&SemanticNode> { self.nodes.get(id as usize) }

    pub fn file_path(&self, id: NodeId) -> Option<&str> {
        let node = self.nodes.get(id as usize)?;
        let file = self.nodes.get(node.file_id as usize)?;
        (file.kind == NodeKind::File).then_some(file.qualified_name.as_str())
    }

    pub fn edge_file_path(&self, edge: &SemanticEdge) -> Option<&str> {
        self.file_path(edge.from)
    }

    pub fn logical_bytes(&self) -> usize {
        let mut bytes = std::mem::size_of::<Self>();
        for node in &self.nodes {
            bytes = bytes.saturating_add(std::mem::size_of::<SemanticNode>())
                .saturating_add(node.name.len()).saturating_add(node.qualified_name.len())
                .saturating_add(node.owner_name.as_ref().map_or(0, String::len))
                .saturating_add(node.receiver_alias.as_ref().map_or(0, String::len));
        }
        bytes = bytes.saturating_add(self.edges.len().saturating_mul(std::mem::size_of::<SemanticEdge>()));
        bytes
    }

    pub fn select_symbols(&self, query: &str, file_filter: Option<&str>) -> Vec<NodeId> {
        let mut ids = BTreeSet::new();
        for map in [&self.by_qualified, &self.by_name] {
            if let Some(found) = map.get(query) {
                for &id in found {
                    if self.nodes[id as usize].kind == NodeKind::File { continue; }
                    if let Some(filter) = file_filter {
                        let Some(file) = self.file_path(id) else { continue; };
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
                    result.push(TraversalStep { root, node: next, via: edge.kind, depth: next_depth, line: edge.line, column: edge.column });
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
                if let Some(file_ids) = self.by_file.get(&node.qualified_name) { out.extend(file_ids.iter().copied()); }
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
            if i == 0 { out.push(PathHop { node, via: None, line: 0, column: 0 }); }
            else {
                let (_, index) = previous[&node]; let edge = &self.edges[index];
                out.push(PathHop { node, via: Some(edge.kind), line: edge.line, column: edge.column });
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
        index.semantic_graph().expect("semantic graph").clone()
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
        assert_eq!(g.file_path(callees[0].node).unwrap(), "src/a.ts");
    }

    #[test]
    fn cross_module_bare_call_creates_no_edge() {
        let g = graph(&[
            ("a.ts", "export function save() {}\n"),
            ("b.ts", "export function save() {}\n"),
            ("c.ts", "export function run(){ save(); }\n"),
        ]);
        let run = g.select_symbols("run", None)[0];
        assert!(g.callees(&[run], 1, 20).is_empty());
        assert!(g.diagnostics().unresolved_calls > 0);
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
    fn bare_call_does_not_bind_to_unrelated_method() {
        let g = graph(&[(
            "a.ts",
            "class Store { save() {} }\nexport function run(){ save(); }\n",
        )]);
        let run = g.select_symbols("run", None)[0];
        assert!(g.callees(&[run], 1, 20).is_empty());
    }

    #[test]
    fn bare_call_does_not_escape_another_lexical_scope() {
        let g = graph(&[(
            "a.ts",
            "function owner(){ function hidden(){} }\nexport function run(){ hidden(); }\n",
        )]);
        let run = g.select_symbols("run", None)[0];
        assert!(g.callees(&[run], 1, 20).is_empty());
    }

    #[test]
    fn sibling_nested_functions_share_their_parent_scope() {
        let g = graph(&[(
            "a.ts",
            "function outer(){ function a(){ b(); } function b(){} a(); }\n",
        )]);
        let a = g.select_symbols("a", None)[0];
        let callees = g.callees(&[a], 1, 20);
        assert_eq!(callees.len(), 1);
        assert_eq!(g.node(callees[0].node).unwrap().name, "b");
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
        assert_eq!(g.file_path(steps[0].node).unwrap(), "util.py");
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
        assert_eq!(g.file_path(steps[0].node).unwrap(), "src/util.rs");
    }

    #[test]
    fn go_same_package_bare_call_resolves_across_files() {
        let g = graph(&[
            ("go.mod", "module example.com/qcfixture\n\ngo 1.24\n"),
            ("a.go", "package main\nfunc Save() {}\n"),
            ("b.go", "package main\nfunc Run(){ Save() }\n"),
        ]);
        let run = g.select_symbols("Run", None)[0];
        let steps = g.callees(&[run], 1, 20);
        assert_eq!(steps.len(), 1);
        assert_eq!(g.node(steps[0].node).unwrap().name, "Save");
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
        assert_eq!(g.file_path(steps[0].node).unwrap(), "util/util.go");
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
        assert_eq!(g.file_path(only_a[0]).unwrap(), "a.ts");
    }
}
