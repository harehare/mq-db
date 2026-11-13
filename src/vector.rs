//! HNSW approximate nearest-neighbour index over cosine similarity.
//!
//! Vectors are normalised on insert, so similarity is a dot product. Nodes are
//! numbered in insertion order; each carries a caller-supplied `key` (the
//! source table's row number) that searches return.
//!
//! Reference: Malkov & Yashunin, "Efficient and robust approximate nearest
//! neighbor search using Hierarchical Navigable Small World graphs".

use crate::MqdbError;
use rustc_hash::FxHashSet;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

const MAGIC: &[u8; 4] = b"MQVI";
const FORMAT_VERSION: u8 = 1;
const NO_ENTRY: u32 = u32::MAX;
const MAX_M: usize = 128;

/// Build parameters. Larger values give better recall and bigger, slower
/// builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswParams {
    /// Neighbours kept per node on upper layers (layer 0 keeps `2 * m`).
    pub m: usize,
    /// Candidate list size while building.
    pub ef_construction: usize,
}

impl Default for HnswParams {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 200,
        }
    }
}

/// A node id ordered by distance (smaller = closer).
#[derive(Clone, Copy, PartialEq)]
struct Candidate {
    dist: f32,
    id: u32,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.dist
            .total_cmp(&other.dist)
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone)]
pub struct HnswIndex {
    dim: usize,
    params: HnswParams,
    entry: Option<u32>,
    max_level: usize,
    /// Normalised vectors, `dim` floats per node.
    vectors: Vec<f32>,
    keys: Vec<u32>,
    /// `links[node][level]` = neighbour node ids.
    links: Vec<Vec<Vec<u32>>>,
    rng: u64,
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Parses a JSON array of numbers such as `[0.1, 0.2]`.
pub fn parse_vector_text(text: &str) -> Option<Vec<f32>> {
    serde_json::from_str::<Vec<f32>>(text.trim()).ok()
}

/// Scales `v` to unit length. `None` for an empty, all-zero or non-finite
/// vector, which has no direction to compare.
pub fn normalized(v: &[f32]) -> Option<Vec<f32>> {
    let norm = dot(v, v).sqrt();
    if v.is_empty() || !norm.is_finite() || norm == 0.0 {
        return None;
    }
    Some(v.iter().map(|x| x / norm).collect())
}

impl HnswIndex {
    pub fn new(dim: usize, params: HnswParams) -> Self {
        let params = HnswParams {
            m: params.m.clamp(2, MAX_M),
            ef_construction: params.ef_construction.max(params.m),
        };
        Self {
            dim,
            params,
            entry: None,
            max_level: 0,
            vectors: Vec::new(),
            keys: Vec::new(),
            links: Vec::new(),
            rng: 0x5EED_1234_ABCD_0001,
        }
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    fn vector(&self, id: u32) -> &[f32] {
        let start = id as usize * self.dim;
        &self.vectors[start..start + self.dim]
    }

    fn dist_to(&self, query: &[f32], id: u32) -> f32 {
        1.0 - dot(query, self.vector(id))
    }

    fn max_links(&self, level: usize) -> usize {
        if level == 0 {
            self.params.m * 2
        } else {
            self.params.m
        }
    }

    fn random_level(&mut self) -> usize {
        let ml = 1.0 / (self.params.m as f64).ln();
        // (0, 1], so ln() is finite.
        let u = ((splitmix64(&mut self.rng) >> 11) as f64 + 1.0) / (1u64 << 53) as f64;
        ((-u.ln() * ml).floor() as usize).min(32)
    }

    /// Adds a vector under `key`. Returns `false`, adding nothing, if its
    /// length differs from the index's or it has no direction (zero or
    /// non-finite).
    pub fn insert(&mut self, key: u32, vector: &[f32]) -> bool {
        if vector.len() != self.dim {
            return false;
        }
        let Some(query) = normalized(vector) else {
            return false;
        };
        let id = self.keys.len() as u32;
        let level = self.random_level();
        self.vectors.extend_from_slice(&query);
        self.keys.push(key);
        self.links.push(vec![Vec::new(); level + 1]);

        let Some(entry) = self.entry else {
            self.entry = Some(id);
            self.max_level = level;
            return true;
        };

        let mut ep = entry;
        for l in (level + 1..=self.max_level).rev() {
            ep = self.greedy(&query, ep, l);
        }
        let mut eps = vec![ep];
        for l in (0..=level.min(self.max_level)).rev() {
            let found = self.search_layer(&query, &eps, self.params.ef_construction, l);
            let chosen = self.select_neighbors(&found, self.params.m);
            self.links[id as usize][l] = chosen.clone();
            for &nb in &chosen {
                self.links[nb as usize][l].push(id);
                if self.links[nb as usize][l].len() > self.max_links(l) {
                    self.prune(nb, l);
                }
            }
            eps = found.iter().map(|c| c.id).collect();
        }
        if level > self.max_level {
            self.max_level = level;
            self.entry = Some(id);
        }
        true
    }

    /// Walks to the neighbour closest to `query` until none is closer.
    fn greedy(&self, query: &[f32], mut cur: u32, level: usize) -> u32 {
        let mut best = self.dist_to(query, cur);
        loop {
            let mut moved = false;
            for &nb in &self.links[cur as usize][level] {
                let d = self.dist_to(query, nb);
                if d < best {
                    best = d;
                    cur = nb;
                    moved = true;
                }
            }
            if !moved {
                return cur;
            }
        }
    }

    /// Best-first search of one layer; returns up to `ef` nodes, nearest first.
    fn search_layer(&self, query: &[f32], eps: &[u32], ef: usize, level: usize) -> Vec<Candidate> {
        let mut visited: FxHashSet<u32> = eps.iter().copied().collect();
        // Min-heap of nodes still to expand, max-heap of the best `ef` found.
        let mut frontier: BinaryHeap<std::cmp::Reverse<Candidate>> = BinaryHeap::new();
        let mut best: BinaryHeap<Candidate> = BinaryHeap::new();
        for &e in eps {
            let c = Candidate {
                dist: self.dist_to(query, e),
                id: e,
            };
            frontier.push(std::cmp::Reverse(c));
            best.push(c);
        }
        while let Some(std::cmp::Reverse(cur)) = frontier.pop() {
            if best.len() >= ef && cur.dist > best.peek().map_or(f32::INFINITY, |c| c.dist) {
                break;
            }
            for &nb in &self.links[cur.id as usize][level] {
                if !visited.insert(nb) {
                    continue;
                }
                let c = Candidate {
                    dist: self.dist_to(query, nb),
                    id: nb,
                };
                if best.len() < ef || c.dist < best.peek().map_or(f32::INFINITY, |w| w.dist) {
                    frontier.push(std::cmp::Reverse(c));
                    best.push(c);
                    if best.len() > ef {
                        best.pop();
                    }
                }
            }
        }
        best.into_sorted_vec()
    }

    /// Keeps up to `m` of `candidates` (nearest first), preferring ones that
    /// are closer to the base point than to any already-kept neighbour so the
    /// links spread over different directions; fills any remainder with the
    /// nearest rejected ones.
    fn select_neighbors(&self, candidates: &[Candidate], m: usize) -> Vec<u32> {
        let mut kept: Vec<Candidate> = Vec::with_capacity(m);
        let mut rejected: Vec<Candidate> = Vec::new();
        for &c in candidates {
            if kept.len() >= m {
                break;
            }
            let diverse = kept
                .iter()
                .all(|k| c.dist < 1.0 - dot(self.vector(c.id), self.vector(k.id)));
            if diverse {
                kept.push(c);
            } else {
                rejected.push(c);
            }
        }
        for c in rejected {
            if kept.len() >= m {
                break;
            }
            kept.push(c);
        }
        kept.into_iter().map(|c| c.id).collect()
    }

    /// Trims `node`'s links on `level` back to the layer's limit.
    fn prune(&mut self, node: u32, level: usize) {
        let base = self.vector(node).to_vec();
        let mut cands: Vec<Candidate> = self.links[node as usize][level]
            .iter()
            .map(|&id| Candidate {
                dist: self.dist_to(&base, id),
                id,
            })
            .collect();
        cands.sort();
        let limit = self.max_links(level);
        self.links[node as usize][level] = self.select_neighbors(&cands, limit);
    }

    /// The `k` nearest keys to `query` as `(key, cosine similarity)`, most
    /// similar first. `ef` (at least `k`) is the candidate list size: larger
    /// is slower and more accurate. Empty if `query` has the wrong length or
    /// no direction.
    pub fn search(&self, query: &[f32], k: usize, ef: usize) -> Vec<(u32, f32)> {
        let Some(entry) = self.entry else {
            return Vec::new();
        };
        if query.len() != self.dim || k == 0 {
            return Vec::new();
        }
        let Some(query) = normalized(query) else {
            return Vec::new();
        };
        let mut ep = entry;
        for l in (1..=self.max_level).rev() {
            ep = self.greedy(&query, ep, l);
        }
        self.search_layer(&query, &[ep], ef.max(k), 0)
            .into_iter()
            .take(k)
            .map(|c| (self.keys[c.id as usize], 1.0 - c.dist))
            .collect()
    }

    /// Serialises the whole index (vectors and graph).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + self.vectors.len() * 4 + self.keys.len() * 40);
        out.extend_from_slice(MAGIC);
        out.push(FORMAT_VERSION);
        out.extend_from_slice(&(self.dim as u32).to_le_bytes());
        out.extend_from_slice(&(self.params.m as u32).to_le_bytes());
        out.extend_from_slice(&(self.params.ef_construction as u32).to_le_bytes());
        out.extend_from_slice(&(self.keys.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.entry.unwrap_or(NO_ENTRY).to_le_bytes());
        out.extend_from_slice(&(self.max_level as u32).to_le_bytes());
        for k in &self.keys {
            out.extend_from_slice(&k.to_le_bytes());
        }
        for x in &self.vectors {
            out.extend_from_slice(&x.to_le_bytes());
        }
        for node in &self.links {
            out.push(node.len() as u8);
            for level in node {
                out.extend_from_slice(&(level.len() as u16).to_le_bytes());
                for nb in level {
                    out.extend_from_slice(&nb.to_le_bytes());
                }
            }
        }
        out
    }

    /// Parses [`HnswIndex::to_bytes`] output, ignoring trailing bytes (page
    /// chains are padded to a whole page). Rejects anything inconsistent
    /// rather than risking an out-of-range access during a search.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, MqdbError> {
        fn bad(msg: &str) -> MqdbError {
            MqdbError::Storage(format!("corrupt vector index: {msg}"))
        }
        struct Reader<'a>(&'a [u8]);
        impl<'a> Reader<'a> {
            fn take(&mut self, n: usize) -> Result<&'a [u8], MqdbError> {
                if self.0.len() < n {
                    return Err(bad("truncated"));
                }
                let (head, tail) = self.0.split_at(n);
                self.0 = tail;
                Ok(head)
            }
            fn u8(&mut self) -> Result<u8, MqdbError> {
                Ok(self.take(1)?[0])
            }
            fn u16(&mut self) -> Result<u16, MqdbError> {
                Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
            }
            fn u32(&mut self) -> Result<u32, MqdbError> {
                Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
            }
        }

        let mut r = Reader(bytes);
        if r.take(4)? != MAGIC {
            return Err(bad("bad magic"));
        }
        if r.u8()? != FORMAT_VERSION {
            return Err(bad("unsupported version"));
        }
        let dim = r.u32()? as usize;
        let m = r.u32()? as usize;
        let ef_construction = r.u32()? as usize;
        let n = r.u32()? as usize;
        let entry = r.u32()?;
        let max_level = r.u32()? as usize;
        if dim == 0 || !(2..=MAX_M).contains(&m) || max_level > 32 {
            return Err(bad("invalid header"));
        }
        // Reject a header that promises more data than the buffer holds
        // before allocating for it.
        let need = n
            .checked_mul(4 + dim * 4)
            .ok_or_else(|| bad("size overflow"))?;
        if r.0.len() < need {
            return Err(bad("truncated"));
        }
        if (entry == NO_ENTRY) != (n == 0) || (entry != NO_ENTRY && entry as usize >= n) {
            return Err(bad("invalid entry point"));
        }

        let mut keys = Vec::with_capacity(n);
        for _ in 0..n {
            keys.push(r.u32()?);
        }
        let mut vectors = Vec::with_capacity(n * dim);
        for _ in 0..n * dim {
            vectors.push(f32::from_le_bytes(r.take(4)?.try_into().unwrap()));
        }
        let mut links = Vec::with_capacity(n);
        for _ in 0..n {
            let levels = r.u8()? as usize;
            if levels == 0 || levels > max_level + 1 {
                return Err(bad("invalid node level"));
            }
            let mut node = Vec::with_capacity(levels);
            for _ in 0..levels {
                let count = r.u16()? as usize;
                let mut level = Vec::with_capacity(count);
                for _ in 0..count {
                    let nb = r.u32()?;
                    if nb as usize >= n {
                        return Err(bad("neighbour out of range"));
                    }
                    level.push(nb);
                }
                node.push(level);
            }
            links.push(node);
        }
        // A link on level L must point at a node that has that level.
        for node in &links {
            for (l, level) in node.iter().enumerate() {
                if level.iter().any(|&nb| links[nb as usize].len() <= l) {
                    return Err(bad("link to a node missing that level"));
                }
            }
        }
        if entry != NO_ENTRY && links[entry as usize].len() != max_level + 1 {
            return Err(bad("entry point is not on the top level"));
        }
        let mut seed = n as u64;
        Ok(Self {
            dim,
            params: HnswParams { m, ef_construction },
            entry: (entry != NO_ENTRY).then_some(entry),
            max_level,
            vectors,
            keys,
            links,
            rng: splitmix64(&mut seed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random vectors in `[-1, 1)`.
    fn random_vectors(n: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                (0..dim)
                    .map(|_| (splitmix64(&mut s) >> 40) as f32 / (1u64 << 23) as f32 - 1.0)
                    .collect()
            })
            .collect()
    }

    fn exact_top_k(data: &[Vec<f32>], query: &[f32], k: usize) -> Vec<u32> {
        let q = normalized(query).unwrap();
        let mut scored: Vec<(u32, f32)> = data
            .iter()
            .enumerate()
            .filter_map(|(i, v)| normalized(v).map(|v| (i as u32, dot(&q, &v))))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.into_iter().take(k).map(|(i, _)| i).collect()
    }

    fn build(data: &[Vec<f32>]) -> HnswIndex {
        let mut idx = HnswIndex::new(data[0].len(), HnswParams::default());
        for (i, v) in data.iter().enumerate() {
            assert!(idx.insert(i as u32, v));
        }
        idx
    }

    fn recall(
        idx: &HnswIndex,
        data: &[Vec<f32>],
        queries: &[Vec<f32>],
        k: usize,
        ef: usize,
    ) -> f64 {
        let mut hit = 0;
        for q in queries {
            let truth = exact_top_k(data, q, k);
            let got: Vec<u32> = idx
                .search(q, k, ef)
                .into_iter()
                .map(|(key, _)| key)
                .collect();
            hit += truth.iter().filter(|t| got.contains(t)).count();
        }
        hit as f64 / (queries.len() * k) as f64
    }

    #[test]
    fn recall_is_high_against_exact_search() {
        let data = random_vectors(3000, 32, 1);
        let queries = random_vectors(50, 32, 2);
        let idx = build(&data);
        assert_eq!(idx.len(), 3000);
        let r = recall(&idx, &data, &queries, 10, 64);
        assert!(r >= 0.95, "recall@10 was {r}");
    }

    #[test]
    fn larger_ef_does_not_reduce_recall() {
        let data = random_vectors(2000, 24, 3);
        let queries = random_vectors(40, 24, 4);
        let idx = build(&data);
        let low = recall(&idx, &data, &queries, 10, 10);
        let high = recall(&idx, &data, &queries, 10, 200);
        assert!(high >= low, "{high} < {low}");
        assert!(high >= 0.98, "recall@10 with ef=200 was {high}");
    }

    #[test]
    fn exact_match_is_first_with_similarity_one() {
        let data = random_vectors(500, 16, 5);
        let idx = build(&data);
        let hits = idx.search(&data[123], 3, 50);
        assert_eq!(hits[0].0, 123);
        assert!((hits[0].1 - 1.0).abs() < 1e-5);
        assert!(
            hits.windows(2).all(|w| w[0].1 >= w[1].1),
            "sorted by similarity"
        );
    }

    #[test]
    fn scale_does_not_matter_for_cosine() {
        let data = random_vectors(300, 8, 6);
        let idx = build(&data);
        let scaled: Vec<f32> = data[7].iter().map(|x| x * 1000.0).collect();
        assert_eq!(idx.search(&scaled, 1, 50)[0].0, 7);
    }

    #[test]
    fn rejects_bad_inserts_and_queries() {
        let mut idx = HnswIndex::new(3, HnswParams::default());
        assert!(!idx.insert(0, &[1.0, 2.0]), "wrong dimension");
        assert!(!idx.insert(0, &[0.0, 0.0, 0.0]), "zero vector");
        assert!(!idx.insert(0, &[f32::NAN, 1.0, 1.0]), "non-finite");
        assert!(idx.is_empty());
        assert!(
            idx.search(&[1.0, 0.0, 0.0], 5, 10).is_empty(),
            "empty index"
        );
        assert!(idx.insert(9, &[1.0, 0.0, 0.0]));
        assert!(
            idx.search(&[1.0, 0.0], 5, 10).is_empty(),
            "wrong query length"
        );
        assert!(idx.search(&[0.0, 0.0, 0.0], 5, 10).is_empty(), "zero query");
        assert!(idx.search(&[1.0, 0.0, 0.0], 0, 10).is_empty(), "k = 0");
        assert_eq!(
            idx.search(&[1.0, 0.0, 0.0], 5, 10),
            vec![(9, 1.0)],
            "k > len"
        );
    }

    #[test]
    fn keys_are_independent_of_node_numbering() {
        let mut idx = HnswIndex::new(2, HnswParams::default());
        idx.insert(100, &[1.0, 0.0]);
        idx.insert(7, &[0.0, 1.0]);
        assert_eq!(idx.search(&[0.1, 1.0], 1, 10)[0].0, 7);
    }

    #[test]
    fn serialisation_round_trips_and_gives_identical_results() {
        let data = random_vectors(800, 16, 7);
        let queries = random_vectors(20, 16, 8);
        let idx = build(&data);
        let mut bytes = idx.to_bytes();
        bytes.extend_from_slice(&[0u8; 4096]); // page padding
        let loaded = HnswIndex::from_bytes(&bytes).unwrap();
        assert_eq!(loaded.len(), idx.len());
        assert_eq!(loaded.dim(), idx.dim());
        for q in &queries {
            assert_eq!(loaded.search(q, 10, 64), idx.search(q, 10, 64));
        }
    }

    #[test]
    fn empty_index_round_trips() {
        let idx = HnswIndex::new(4, HnswParams::default());
        let loaded = HnswIndex::from_bytes(&idx.to_bytes()).unwrap();
        assert!(loaded.is_empty());
        assert_eq!(loaded.dim(), 4);
    }

    #[test]
    fn corrupt_input_is_rejected_not_trusted() {
        let data = random_vectors(200, 8, 9);
        let good = build(&data).to_bytes();
        assert!(HnswIndex::from_bytes(&[]).is_err());
        assert!(HnswIndex::from_bytes(b"NOPE").is_err());
        let mut bad_magic = good.clone();
        bad_magic[0] = b'X';
        assert!(HnswIndex::from_bytes(&bad_magic).is_err());
        assert!(
            HnswIndex::from_bytes(&good[..good.len() / 2]).is_err(),
            "truncated"
        );
        // Every truncation must be an error, never a panic.
        for cut in (0..good.len()).step_by(97) {
            let _ = HnswIndex::from_bytes(&good[..cut]);
        }
        // A header promising far more nodes than the buffer holds.
        let mut huge = good.clone();
        huge[17..21].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(HnswIndex::from_bytes(&huge).is_err());
        // Flip bytes throughout: parsing may succeed or fail but a search on
        // whatever parses must not panic.
        for i in (30..good.len()).step_by(53) {
            let mut mutated = good.clone();
            mutated[i] ^= 0xFF;
            if let Ok(idx) = HnswIndex::from_bytes(&mutated) {
                let _ = idx.search(&data[0], 5, 20);
            }
        }
    }

    #[test]
    fn parse_vector_text_accepts_json_arrays_only() {
        assert_eq!(
            parse_vector_text(" [1, 2.5, -3] "),
            Some(vec![1.0, 2.5, -3.0])
        );
        assert_eq!(parse_vector_text("[]"), Some(vec![]));
        assert_eq!(parse_vector_text("1,2"), None);
        assert_eq!(parse_vector_text("[\"a\"]"), None);
        assert_eq!(parse_vector_text(""), None);
    }
}
