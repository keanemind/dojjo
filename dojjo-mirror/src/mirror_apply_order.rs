//! **Pull apply order** for paths under `mirror/`: dependencies before dependents (especially op DAG).
//!
//! Layers (strictest → loosest for unknown paths):
//! 1. `store/**` — object backend bytes before anything that references commits/trees.
//! 2. `op_store/views/**` — view blobs before operations that reference `view_id`.
//! 3. `op_store/type` — op-store backend marker.
//! 4. `op_store/operations/**` — topological order by decoded `Operation.parents` edges (parents first).
//! 5. **Misc** — any other path (sorted); avoid in production JJ mirrors.
//! 6. `index/**`, `submodules/**` — optional caches / git submodule data.
//! 7. `op_heads/**` — last: publication of heads after all referenced ops/views/store exist.
//!
//! **Push** should mirror this ordering; **pull** must follow it when writing a live `.jj/repo`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use jj_lib::protos::simple_op_store::Operation as OperationProto;
use prost::Message;

/// BLAKE2b-512 operation id length in bytes (see `jj` `simple_op_store`).
const OPERATION_ID_BYTES: usize = 64;
/// Hex-encoded operation id file name length.
const OPERATION_ID_HEX_CHARS: usize = OPERATION_ID_BYTES * 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Layer {
    Store = 0,
    Views = 1,
    OpStoreType = 2,
    Operations = 3,
    Misc = 4,
    Index = 5,
    Submodules = 6,
    OpHeads = 7,
}

fn classify_path(path: &str) -> Layer {
    assert!(!path.is_empty(), "path must not be empty");
    if path.starts_with("store/") {
        return Layer::Store;
    }
    if path.starts_with("op_store/views/") {
        return Layer::Views;
    }
    if path == "op_store/type" {
        return Layer::OpStoreType;
    }
    if path.starts_with("op_store/operations/") {
        return Layer::Operations;
    }
    if path.starts_with("index/") {
        return Layer::Index;
    }
    if path.starts_with("submodules/") {
        return Layer::Submodules;
    }
    if path.starts_with("op_heads/") {
        return Layer::OpHeads;
    }
    Layer::Misc
}

fn is_all_zero(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == 0)
}

fn hex_id_valid(s: &str) -> bool {
    s.len() == OPERATION_ID_HEX_CHARS && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Build edges for `op_store/operations/*` from on-disk protobuf; parents before children.
fn operation_parent_edges(mirror_root: &Path, op_paths: &[String]) -> Result<Vec<(String, String)>, std::io::Error> {
    assert!(
        mirror_root.as_os_str().len() > 0,
        "mirror_root must not be empty"
    );
    let set: HashSet<&str> = op_paths.iter().map(|s| s.as_str()).collect();
    let mut edges = Vec::new();

    for child in op_paths {
        let buf = std::fs::read(mirror_root.join(child))?;
        let proto = OperationProto::decode(&*buf).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad operation proto: {e}"))
        })?;

        let mut unique_parents: HashSet<Vec<u8>> = HashSet::new();
        for parent in proto.parents {
            if parent.len() != OPERATION_ID_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "operation parent id has wrong byte length",
                ));
            }
            if is_all_zero(&parent) {
                continue;
            }
            if !unique_parents.insert(parent.clone()) {
                continue;
            }
            let parent_hex = hex::encode(&parent);
            assert_eq!(
                parent_hex.len(),
                OPERATION_ID_HEX_CHARS,
                "hex length matches operation id"
            );
            let parent_path = format!("op_store/operations/{parent_hex}");
            if !set.contains(parent_path.as_str()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("missing parent operation file: {parent_path}"),
                ));
            }
            if parent_path == *child {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "operation must not be its own parent",
                ));
            }
            edges.push((parent_path, child.clone()));
        }
    }

    Ok(edges)
}

fn kahn_toposort(nodes: BTreeSet<String>, edges: Vec<(String, String)>) -> Result<Vec<String>, &'static str> {
    let mut indegree: HashMap<String, usize> = HashMap::new();
    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for n in &nodes {
        indegree.insert(n.clone(), 0);
    }
    for (u, v) in edges {
        assert!(nodes.contains(&u), "edge tail must be in node set");
        assert!(nodes.contains(&v), "edge head must be in node set");
        adj.entry(u).or_default().push(v.clone());
        *indegree.entry(v).or_insert(0) += 1;
    }

    let mut ready: BTreeSet<String> = nodes
        .iter()
        .filter(|n| *indegree.get(*n).unwrap_or(&0) == 0)
        .cloned()
        .collect();
    let mut out = Vec::with_capacity(nodes.len());

    while let Some(n) = ready.iter().next().cloned() {
        ready.remove(&n);
        out.push(n.clone());
        for w in adj.get(&n).into_iter().flatten() {
            let d = indegree.get_mut(w).expect("indegree for edge head");
            assert!(*d > 0, "indegree must be positive before decrement");
            *d -= 1;
            if *d == 0 {
                ready.insert(w.clone());
            }
        }
    }

    if out.len() != nodes.len() {
        return Err("operation graph has a cycle or disconnected node with missing parents");
    }
    Ok(out)
}

fn sorted_paths_in_layer(paths: &[String], layer: Layer) -> Vec<String> {
    let mut v: Vec<String> = paths
        .iter()
        .filter(|p| classify_path(p) == layer)
        .cloned()
        .collect();
    v.sort();
    assert!(
        v.windows(2).all(|w| w[0] <= w[1]),
        "sorted_paths_in_layer must produce non-decreasing order"
    );
    v
}

/// Full **pull apply** sequence: same relative paths as in `entries`, ordered for safe materialization.
pub fn compute_mirror_apply_order(mirror_root: &Path, paths: &[String]) -> Result<Vec<String>, std::io::Error> {
    assert!(
        mirror_root.as_os_str().len() > 0,
        "mirror_root must not be non-empty"
    );

    let mut out = Vec::with_capacity(paths.len());

    for layer in [
        Layer::Store,
        Layer::Views,
        Layer::OpStoreType,
        Layer::Operations,
        Layer::Misc,
        Layer::Index,
        Layer::Submodules,
        Layer::OpHeads,
    ] {
        if layer == Layer::Operations {
            let op_paths: Vec<String> = paths
                .iter()
                .filter(|p| classify_path(p) == Layer::Operations)
                .cloned()
                .collect();

            for p in &op_paths {
                let rest = p
                    .strip_prefix("op_store/operations/")
                    .expect("operations layer paths must have prefix");
                if !hex_id_valid(rest) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid operation id filename: {p}"),
                    ));
                }
            }

            if op_paths.is_empty() {
                continue;
            }

            let edges = operation_parent_edges(mirror_root, &op_paths)?;
            let nodes: BTreeSet<String> = op_paths.iter().cloned().collect();
            let sorted_ops = kahn_toposort(nodes, edges).map_err(|msg| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
            })?;
            assert_eq!(
                sorted_ops.len(),
                op_paths.len(),
                "toposort must include every operation path"
            );
            out.extend(sorted_ops);
            continue;
        }

        out.extend(sorted_paths_in_layer(paths, layer));
    }

    assert!(
        out.len() <= paths.len(),
        "apply order must not invent new paths"
    );
    let out_set: HashSet<&str> = out.iter().map(|s| s.as_str()).collect();
    let path_set: HashSet<&str> = paths.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        out_set, path_set,
        "apply order must be a permutation of input paths"
    );

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kahn_detects_cycle() {
        let nodes: BTreeSet<String> = ["a".into(), "b".into()].into_iter().collect();
        let edges = vec![("a".into(), "b".into()), ("b".into(), "a".into())];
        assert!(kahn_toposort(nodes, edges).is_err());
    }

    #[test]
    fn kahn_linear() {
        let nodes: BTreeSet<String> = ["a".into(), "b".into(), "c".into()].into_iter().collect();
        let edges = vec![("a".into(), "b".into()), ("b".into(), "c".into())];
        let got = kahn_toposort(nodes, edges).unwrap();
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[test]
    fn classify_layers() {
        let hex = "a".repeat(OPERATION_ID_HEX_CHARS);
        assert_eq!(classify_path("store/git_target"), Layer::Store);
        assert_eq!(classify_path("op_store/views/ab"), Layer::Views);
        assert_eq!(classify_path("op_store/type"), Layer::OpStoreType);
        assert_eq!(
            classify_path(&format!("op_store/operations/{hex}")),
            Layer::Operations
        );
        assert_eq!(classify_path("op_heads/heads"), Layer::OpHeads);
        assert_eq!(classify_path("misc/foo"), Layer::Misc);
    }
}
