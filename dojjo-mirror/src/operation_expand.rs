//! Include transitive `Operation.parents` on disk so [`crate::mirror_apply_order`] can toposort a partial op set.

use std::collections::HashSet;
use std::path::Path;

use jj_lib::protos::simple_op_store::Operation as OperationProto;
use prost::Message;

const OP_PREFIX: &str = "op_store/operations/";
const OPERATION_ID_BYTES: usize = 64;
const OPERATION_ID_HEX_CHARS: usize = OPERATION_ID_BYTES * 2;

fn is_all_zero(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| b == 0)
}

/// `seed_paths` plus every `op_store/operations/*` parent reachable in the on-disk op DAG.
///
/// Needed because `compute_mirror_apply_order` requires every referenced parent operation file
/// to appear in its input path list.
pub fn expand_operation_parent_closure(repo_root: &Path, seed_paths: &[String]) -> std::io::Result<Vec<String>> {
    assert!(
        repo_root.as_os_str().len() > 0,
        "repo_root path must not be empty"
    );

    let mut all: HashSet<String> = seed_paths.iter().cloned().collect();
    let mut stack: Vec<String> = seed_paths
        .iter()
        .filter(|p| p.starts_with(OP_PREFIX))
        .cloned()
        .collect();

    while let Some(child) = stack.pop() {
        assert!(
            child.starts_with(OP_PREFIX),
            "stack must only hold operation paths"
        );
        let buf = std::fs::read(repo_root.join(&child))?;
        let proto = OperationProto::decode(&*buf).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad operation proto: {e}"))
        })?;

        let mut unique_parent_ids: HashSet<Vec<u8>> = HashSet::new();
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
            if !unique_parent_ids.insert(parent.clone()) {
                continue;
            }
            let parent_hex = hex::encode(&parent);
            assert_eq!(
                parent_hex.len(),
                OPERATION_ID_HEX_CHARS,
                "parent hex length invariant"
            );
            let parent_path = format!("{OP_PREFIX}{parent_hex}");
            if all.contains(&parent_path) {
                continue;
            }
            let abs = repo_root.join(&parent_path);
            if !abs.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "operation {child} references parent op file not on disk: {parent_path}"
                    ),
                ));
            }
            all.insert(parent_path.clone());
            stack.push(parent_path);
        }
    }

    let mut out: Vec<String> = all.into_iter().collect();
    out.sort();
    assert!(
        out.windows(2).all(|w| w[0] <= w[1]),
        "closure output must be sorted"
    );
    Ok(out)
}
