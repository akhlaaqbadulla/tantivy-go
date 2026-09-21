use serde::{Deserialize, Serialize};

#[derive(Serialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum QueryType {
    BoolQuery,
    PhraseQuery,
    PhrasePrefixQuery,
    TermPrefixQuery,
    TermQuery,
    EveryTermQuery,
    OneOfTermQuery,
    AllQuery,
    FuzzyTermQuery,
    OneOfFuzzyTermQuery,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum QueryModifier {
    Must,
    Should,
    MustNot,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum GoQuery {
    BoolQuery {
        subqueries: Vec<QueryElement>,
        boost: f32,
    },
    PhraseQuery {
        field_index: usize,
        text_index: usize,
        boost: f32,
    },
    PhrasePrefixQuery {
        field_index: usize,
        text_index: usize,
        boost: f32,
    },
    TermPrefixQuery {
        field_index: usize,
        text_index: usize,
        boost: f32,
    },
    TermQuery {
        field_index: usize,
        text_index: usize,
        boost: f32,
    },
    EveryTermQuery {
        field_index: usize,
        text_index: usize,
        boost: f32,
    },
    OneOfTermQuery {
        field_index: usize,
        text_index: usize,
        boost: f32,
    },
    AllQuery {
        boost: f32,
    },
    /// Typo-tolerant match on a SINGLE token.
    ///
    /// The text is run through the field's analyzer and its first term is
    /// expanded against the segment term dictionaries with a Levenshtein
    /// automaton. The expansion produces ordinary `TermQuery`s, NOT tantivy's
    /// `FuzzyTermQuery`: that one returns an `AutomatonWeight` whose scorer is
    /// a `ConstScorer`, so every fuzzy match scores identically and BM25
    /// ranking is destroyed. Expanding to real terms keeps scoring intact and
    /// lets each alternative carry a boost that reflects its edit distance.
    FuzzyTermQuery {
        field_index: usize,
        text_index: usize,
        /// Maximum Levenshtein distance, 0..=2. Capped per token by length.
        distance: u8,
        /// Damerau-Levenshtein: count a transposition as one edit, not two.
        transposition: bool,
        /// Match on a prefix of the indexed term rather than the whole term.
        prefix: bool,
        boost: f32,
    },
    /// Typo-tolerant match over EVERY token of the text.
    ///
    /// Mirrors `OneOfTermQuery`'s positional weighting, with each token's
    /// exact term and its fuzzy alternatives nested INSIDE that token's slot.
    /// Nesting matters: flattening the alternatives into one sibling set lets a
    /// distance-1 hit on the first token outrank an exact hit on the last.
    OneOfFuzzyTermQuery {
        field_index: usize,
        text_index: usize,
        distance: u8,
        transposition: bool,
        prefix: bool,
        boost: f32,
    },
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct QueryElement {
    pub query: Option<GoQuery>,
    pub modifier: QueryModifier,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct BoolQuery {
    pub subqueries: Vec<QueryElement>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct FinalQuery {
    pub texts: Vec<String>,
    pub fields: Vec<String>,
    pub query: BoolQuery,
}
