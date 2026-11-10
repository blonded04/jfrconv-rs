// Portions derived from async-profiler 4.1. Copyright The async-profiler authors.
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
pub struct Frame {
    pub name: u32,
    pub kind: u8,
    pub line: i32,
    pub bci: i32,
    pub role: u8,
}

#[derive(Clone, Debug, Default)]
pub struct Node {
    pub parent: u32,
    pub frame: u32,
    pub total: u64,
    pub self_value: u64,
    pub inlined: u64,
    pub c1: u64,
    pub interpreted: u64,
    pub samples: u64,
    pub value: u64,
    pub time_ns: u64,
}

pub struct Profile {
    /// v4.1 retains accepted input stack depth before --skip for canvas sizing.
    pub original_depth: usize,
    /// Direct HTML imports use HashMap.put encounter order; rebuilt paths use
    /// computeIfAbsent, whose collision insertion order is reversed.
    pub html_encounter_order: bool,
    /// HTML's original cpool IDs for interned names only. Unused cpool strings
    /// need not be retained after parsing.
    pub source_title_ids: Option<Vec<u32>>,
    pub is_diff: bool,
    pub diffs: Option<Vec<Option<i64>>>,
    pub strings: Vec<Arc<str>>,
    pub frames: Vec<Frame>,
    pub nodes: Vec<Node>,
    pub start_ns: u64,
    pub end_ns: u64,
    pub value_type: String,
    pub unit: String,
    pub total_unit: String,
    names: HashMap<Arc<str>, u32>,
    frame_ids: HashMap<Frame, u32>,
    edges: HashMap<(u32, u32), u32>,
}

#[derive(Clone, Debug)]
pub struct ExportOptions {
    pub title: String,
    pub minwidth: f64,
    pub reverse: bool,
    pub inverted: bool,
    pub highlight: Option<String>,
}

/// Only the differing Java execution kinds need to be retained in a cached path.
#[derive(Clone, Debug, Default)]
pub struct CachedPath {
    pub leaf: u32,
    subtypes: Box<[(u32, u8)]>,
}

impl Profile {
    pub fn new(value_type: &str, unit: &str, total_unit: &str) -> Self {
        let empty: Arc<str> = Arc::from("");
        let root = Frame {
            name: 0,
            kind: 3,
            line: 0,
            bci: 0,
            role: 0,
        };
        Self {
            original_depth: 0,
            html_encounter_order: false,
            source_title_ids: None,
            is_diff: false,
            diffs: None,
            strings: vec![empty.clone()],
            frames: vec![root],
            nodes: vec![Node::default()],
            start_ns: 0,
            end_ns: 0,
            value_type: value_type.into(),
            unit: unit.into(),
            total_unit: total_unit.into(),
            names: HashMap::from([(empty, 0)]),
            frame_ids: HashMap::from([(root, 0)]),
            edges: HashMap::new(),
        }
    }

    pub fn intern(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.names.get(name) {
            return id;
        }
        let s: Arc<str> = Arc::from(name);
        let id = u32::try_from(self.strings.len()).expect("too many strings");
        self.strings.push(s.clone());
        self.names.insert(s, id);
        if let Some(ids) = &mut self.source_title_ids {
            ids.push(u32::MAX);
        }
        id
    }

    pub fn name_id(&self, name: &str) -> Option<u32> {
        self.names.get(name).copied()
    }

    pub fn frame(&mut self, name: &str, kind: u8, line: i32, role: u8) -> u32 {
        self.frame_location(name, kind, line, 0, role)
    }

    pub fn frame_location(&mut self, name: &str, kind: u8, line: i32, bci: i32, role: u8) -> u32 {
        let name = self.intern(name);
        self.frame_key(Frame {
            name,
            kind,
            line,
            bci,
            role,
        })
    }

    fn frame_key(&mut self, key: Frame) -> u32 {
        if let Some(&id) = self.frame_ids.get(&key) {
            return id;
        }
        let id = u32::try_from(self.frames.len()).expect("too many frames");
        self.frames.push(key);
        self.frame_ids.insert(key, id);
        id
    }

    pub fn path(&mut self, frames: &[u32]) -> CachedPath {
        self.path_impl(frames, true)
    }

    pub fn path_exact(&mut self, frames: &[u32]) -> CachedPath {
        self.path_impl(frames, false)
    }

    fn path_impl(&mut self, frames: &[u32], normalize: bool) -> CachedPath {
        self.original_depth = self.original_depth.max(frames.len());
        let mut parent = 0;
        let mut subtypes = Vec::new();
        for &id in frames {
            let mut frame = self.frames[id as usize];
            let original_kind = frame.kind;
            if normalize && matches!(frame.kind, 0 | 2 | 6) {
                frame.kind = 1;
            }
            let normalized = self.frame_key(frame);
            let node_id = if let Some(&node) = self.edges.get(&(parent, normalized)) {
                node
            } else {
                let node = u32::try_from(self.nodes.len()).expect("too many stack prefixes");
                self.nodes.push(Node {
                    parent,
                    frame: normalized,
                    ..Node::default()
                });
                self.edges.insert((parent, normalized), node);
                node
            };
            if normalize && matches!(original_kind, 0 | 2 | 6) {
                subtypes.push((node_id, original_kind));
            }
            parent = node_id;
        }
        CachedPath {
            leaf: parent,
            subtypes: subtypes.into_boxed_slice(),
        }
    }

    pub fn credit(
        &mut self,
        path: &CachedPath,
        amount: u64,
        samples: u64,
        value: u64,
        time_ns: u64,
    ) {
        let leaf = &mut self.nodes[path.leaf as usize];
        leaf.self_value = leaf.self_value.saturating_add(amount);
        leaf.samples = leaf.samples.saturating_add(samples);
        leaf.value = leaf.value.saturating_add(value);
        if leaf.time_ns == 0 || time_ns < leaf.time_ns {
            leaf.time_ns = time_ns;
        }
        let mut id = path.leaf;
        loop {
            let node = &mut self.nodes[id as usize];
            node.total = node.total.saturating_add(amount);
            if id == 0 {
                break;
            }
            id = node.parent;
        }
        for &(id, kind) in path.subtypes.iter() {
            let node = &mut self.nodes[id as usize];
            let counter = match kind {
                0 => &mut node.interpreted,
                2 => &mut node.inlined,
                _ => &mut node.c1,
            };
            *counter = counter.saturating_add(amount);
        }
    }

    pub fn credit_leaf(&mut self, leaf: u32, amount: u64, samples: u64, value: u64, time_ns: u64) {
        let path = CachedPath {
            leaf,
            ..CachedPath::default()
        };
        self.credit(&path, amount, samples, value, time_ns);
    }

    /// Drop construction indexes before exporting; the compact tree is sufficient.
    pub fn seal(&mut self) {
        self.names = HashMap::new();
        self.frame_ids = HashMap::new();
        self.edges = HashMap::new();
    }

    /// Match normalized frame keys along each current path. Like upstream,
    /// disappeared base-only paths are absent; a new path has an undefined delta.
    pub fn diff(&mut self, base: &Self) {
        let mut matches = vec![None; self.nodes.len()];
        matches[0] = Some(0u32);
        let mut diffs = Vec::with_capacity(self.nodes.len());
        for (id, node) in self.nodes.iter().enumerate() {
            if id > 0 {
                let frame = self.frames[node.frame as usize];
                if let Some(parent) = matches[node.parent as usize] {
                    if let Some(&name) = base.names.get(&*self.strings[frame.name as usize]) {
                        let key = Frame { name, ..frame };
                        if let Some(&frame) = base.frame_ids.get(&key) {
                            matches[id] = base.edges.get(&(parent, frame)).copied();
                        }
                    }
                }
            }
            diffs.push(matches[id].map(|id| {
                (node.self_value as i128 - base.nodes[id as usize].self_value as i128)
                    .clamp(i64::MIN as i128, i64::MAX as i128) as i64
            }));
        }
        self.is_diff = true;
        self.diffs = Some(diffs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shares_prefixes_and_preserves_java_type_counts() {
        let mut p = Profile::new("cpu", "count", "nanoseconds");
        let a = p.frame("Main.run", 1, 0, 0);
        let b = p.frame("Child.run", 2, 0, 0);
        let c = p.frame("Child.run", 0, 0, 0);
        let x = p.path(&[a, b]);
        let y = p.path(&[a, c]);
        assert_eq!(x.leaf, y.leaf);
        p.credit(&x, 3, 3, 3, 10);
        p.credit(&y, 2, 2, 2, 20);
        assert_eq!(p.nodes.len(), 3);
        assert_eq!(p.nodes[0].total, 5);
        let n = &p.nodes[x.leaf as usize];
        assert_eq!((n.self_value, n.inlined, n.interpreted), (5, 3, 2));
    }
}
