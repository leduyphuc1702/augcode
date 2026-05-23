use std::collections::HashMap;

const K1: f32 = 1.2;
const B: f32 = 0.75;

#[derive(Debug, Clone, Default)]
pub struct LexicalIndex {
    /// term -> doc_id -> term frequency
    inverted: HashMap<String, HashMap<String, usize>>,
    /// doc_id -> total term count
    doc_lengths: HashMap<String, usize>,
    /// cached average doc length
    avg_dl: f32,
    dirty: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub doc_id: String,
    pub score: f32,
}

impl LexicalIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_document(&mut self, doc_id: &str, text: &str) {
        self.remove_document(doc_id);
        let terms = tokenize(text);
        let mut local_tf: HashMap<String, usize> = HashMap::new();
        for term in &terms {
            *local_tf.entry(term.clone()).or_insert(0) += 1;
        }
        for (term, tf) in local_tf {
            self.inverted
                .entry(term)
                .or_default()
                .insert(doc_id.to_string(), tf);
        }
        self.doc_lengths.insert(doc_id.to_string(), terms.len());
        self.dirty = true;
    }

    pub fn remove_document(&mut self, doc_id: &str) {
        for postings in self.inverted.values_mut() {
            postings.remove(doc_id);
        }
        self.doc_lengths.remove(doc_id);
        self.dirty = true;
    }

    pub fn search(&mut self, query: &str, top_k: usize) -> Vec<SearchHit> {
        if self.dirty {
            self.recompute_avg_dl();
            self.dirty = false;
        }
        let query_terms = tokenize(query);
        if query_terms.is_empty() {
            return Vec::new();
        }
        let n = self.doc_lengths.len() as f32;
        let avg_dl = self.avg_dl.max(1.0);

        let mut scores: HashMap<String, f32> = HashMap::new();
        for term in query_terms {
            let Some(postings) = self.inverted.get(&term) else {
                continue;
            };
            let df = postings.len() as f32;
            let idf = ((n - df + 0.5) / (df + 0.5)).ln().max(0.0);
            for (doc_id, tf) in postings {
                let dl = *self.doc_lengths.get(doc_id).unwrap_or(&0) as f32;
                let tf_f = *tf as f32;
                let denom = tf_f + K1 * (1.0 - B + B * dl / avg_dl);
                let bm25 = idf * (tf_f * (K1 + 1.0)) / denom.max(1e-6);
                *scores.entry(doc_id.clone()).or_insert(0.0) += bm25;
            }
        }

        let mut hits: Vec<SearchHit> = scores
            .into_iter()
            .map(|(doc_id, score)| SearchHit { doc_id, score })
            .collect();
        hits.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        hits.truncate(top_k);
        hits
    }

    fn recompute_avg_dl(&mut self) {
        if self.doc_lengths.is_empty() {
            self.avg_dl = 0.0;
            return;
        }
        let total: usize = self.doc_lengths.values().sum();
        self.avg_dl = total as f32 / self.doc_lengths.len() as f32;
    }
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .filter_map(|term| {
            let term = term.trim().to_lowercase();
            (term.len() >= 2).then_some(term)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_ranks_higher_than_partial() {
        let mut index = LexicalIndex::new();
        index.add_document("doc_a", "the quick brown fox jumps over the lazy dog");
        index.add_document("doc_b", "the slow red fox jumps over the lazy dog");
        let hits = index.search("quick brown", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_id, "doc_a");
    }

    #[test]
    fn delete_removes_from_results() {
        let mut index = LexicalIndex::new();
        index.add_document("doc_a", "hello world");
        index.remove_document("doc_a");
        let hits = index.search("hello", 10);
        assert!(hits.is_empty());
    }

    #[test]
    fn empty_query_returns_empty() {
        let mut index = LexicalIndex::new();
        index.add_document("doc_a", "hello world");
        let hits = index.search("", 10);
        assert!(hits.is_empty());
    }

    #[test]
    fn bm25_rare_term_boosts_relevant_doc() {
        let mut index = LexicalIndex::new();
        index.add_document("common", "the quick brown fox jumps over the lazy dog");
        index.add_document(
            "rare",
            "the quick brown fox jumps over the lazy dog with aardvark",
        );
        index.add_document("other", "the quick brown fox jumps over the lazy dog again");
        let hits = index.search("aardvark", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_id, "rare");
    }
}
