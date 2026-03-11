use std::collections::HashMap;
use std::path::Path;

use hnsw_rs::prelude::AnnT;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::index::{self, HnswIndex};

/// Metadata sidecar for HNSW snapshots.
#[derive(Debug, Serialize, Deserialize)]
pub struct HnswSnapshotMeta {
    pub version: u32,
    pub entity_count: usize,
    pub lsn: u64,
    pub checksum: String,
    pub max_nb_connection: usize,
    pub max_elements: usize,
    pub ef_construction: usize,
    pub dimensions: usize,
}

/// ID mapping state persisted alongside the hnsw_rs graph.
#[derive(Debug, Serialize, Deserialize)]
pub struct HnswIdMap {
    pub id_to_idx: HashMap<String, usize>,
    pub idx_to_id: HashMap<usize, String>,
    pub next_idx: usize,
    pub tombstones: Vec<String>,
}

/// Save an HNSW index snapshot to disk.
/// Uses atomic write: writes to temp dir, then renames into final location.
pub fn save_snapshot(
    dir: &Path,
    collection: &str,
    repr_name: &str,
    hnsw_index: &HnswIndex,
    lsn: u64,
) -> Result<(), String> {
    let basename = format!("{collection}_{repr_name}");

    // Use a temp directory for atomic write
    let tmp_dir = dir.join(format!(".tmp_{basename}"));
    std::fs::create_dir_all(&tmp_dir)
        .map_err(|e| format!("create tmp dir failed: {e}"))?;

    // Step 1: Dump hnsw_rs graph + data via AnnT::file_dump (public API)
    {
        let hnsw = hnsw_index.inner().lock().unwrap();
        hnsw.file_dump(&tmp_dir, &basename)
            .map_err(|e| format!("hnsw file_dump failed: {e}"))?;
    }

    // Step 2: Save ID maps
    let (id_to_idx, idx_to_id, next_idx, tombstones) = hnsw_index.snapshot_id_maps();
    let idmap = HnswIdMap {
        id_to_idx,
        idx_to_id,
        next_idx,
        tombstones,
    };
    let idmap_bytes = bincode::serialize(&idmap)
        .map_err(|e| format!("idmap serialisation failed: {e}"))?;
    std::fs::write(tmp_dir.join(format!("{basename}.hnsw.idmap")), &idmap_bytes)
        .map_err(|e| format!("idmap write failed: {e}"))?;

    // Step 3: Compute checksum over graph + data + idmap files
    let graph_bytes = std::fs::read(tmp_dir.join(format!("{basename}.hnsw.graph")))
        .map_err(|e| format!("checksum read graph: {e}"))?;
    let data_bytes = std::fs::read(tmp_dir.join(format!("{basename}.hnsw.data")))
        .map_err(|e| format!("checksum read data: {e}"))?;
    let mut hasher = Sha256::new();
    hasher.update(&graph_bytes);
    hasher.update(&data_bytes);
    hasher.update(&idmap_bytes);
    let checksum = format!("sha256:{:x}", hasher.finalize());

    // Step 4: Save metadata
    let meta = HnswSnapshotMeta {
        version: 1,
        entity_count: hnsw_index.len(),
        lsn,
        checksum,
        max_nb_connection: index::MAX_NB_CONNECTION,
        max_elements: index::MAX_ELEMENTS,
        ef_construction: index::EF_CONSTRUCTION,
        dimensions: hnsw_index.dimensions(),
    };
    let meta_json = serde_json::to_string_pretty(&meta)
        .map_err(|e| format!("meta serialisation failed: {e}"))?;
    std::fs::write(tmp_dir.join(format!("{basename}.hnsw.meta")), &meta_json)
        .map_err(|e| format!("meta write failed: {e}"))?;

    // Step 5: Atomic rename — move files from temp dir to final location
    for ext in &["hnsw.graph", "hnsw.data", "hnsw.idmap", "hnsw.meta"] {
        let src = tmp_dir.join(format!("{basename}.{ext}"));
        let dst = dir.join(format!("{basename}.{ext}"));
        std::fs::rename(&src, &dst)
            .map_err(|e| format!("atomic rename {ext} failed: {e}"))?;
    }
    let _ = std::fs::remove_dir(&tmp_dir);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LogicalId;

    fn make_id(s: &str) -> LogicalId {
        LogicalId::from_string(s)
    }

    #[test]
    fn snapshot_save_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        let index = HnswIndex::new(3);
        index.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        index.insert(&make_id("e2"), &[0.0, 1.0, 0.0]);

        let result = save_snapshot(dir.path(), "test_collection", "default", &index, 100);
        assert!(result.is_ok(), "save_snapshot should succeed: {:?}", result);

        // Verify files exist
        assert!(dir.path().join("test_collection_default.hnsw.graph").exists());
        assert!(dir.path().join("test_collection_default.hnsw.data").exists());
        assert!(dir.path().join("test_collection_default.hnsw.idmap").exists());
        assert!(dir.path().join("test_collection_default.hnsw.meta").exists());
    }

    #[test]
    fn snapshot_meta_contains_correct_values() {
        let dir = tempfile::tempdir().unwrap();
        let index = HnswIndex::new(3);
        index.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        index.insert(&make_id("e2"), &[0.0, 1.0, 0.0]);
        index.insert(&make_id("e3"), &[0.0, 0.0, 1.0]);

        save_snapshot(dir.path(), "my_coll", "emb", &index, 42).unwrap();

        let meta_str = std::fs::read_to_string(dir.path().join("my_coll_emb.hnsw.meta")).unwrap();
        let meta: HnswSnapshotMeta = serde_json::from_str(&meta_str).unwrap();

        assert_eq!(meta.version, 1);
        assert_eq!(meta.entity_count, 3);
        assert_eq!(meta.lsn, 42);
        assert_eq!(meta.dimensions, 3);
        assert!(meta.checksum.starts_with("sha256:"));
    }

    #[test]
    fn snapshot_idmap_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let index = HnswIndex::new(3);
        index.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        index.insert(&make_id("e2"), &[0.0, 1.0, 0.0]);

        save_snapshot(dir.path(), "coll", "rep", &index, 0).unwrap();

        let idmap_bytes = std::fs::read(dir.path().join("coll_rep.hnsw.idmap")).unwrap();
        let idmap: HnswIdMap = bincode::deserialize(&idmap_bytes).unwrap();

        assert_eq!(idmap.id_to_idx.len(), 2);
        assert_eq!(idmap.idx_to_id.len(), 2);
        assert_eq!(idmap.next_idx, 2);
        assert!(idmap.tombstones.is_empty());
        assert!(idmap.id_to_idx.contains_key("e1"));
        assert!(idmap.id_to_idx.contains_key("e2"));
    }

    #[test]
    fn snapshot_preserves_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let index = HnswIndex::new(3);
        index.insert(&make_id("e1"), &[1.0, 0.0, 0.0]);
        index.insert(&make_id("e2"), &[0.0, 1.0, 0.0]);
        index.remove(&make_id("e1"));

        save_snapshot(dir.path(), "coll", "rep", &index, 0).unwrap();

        let idmap_bytes = std::fs::read(dir.path().join("coll_rep.hnsw.idmap")).unwrap();
        let idmap: HnswIdMap = bincode::deserialize(&idmap_bytes).unwrap();

        assert_eq!(idmap.tombstones.len(), 1);
        assert!(idmap.tombstones.contains(&"e1".to_string()));
    }
}
