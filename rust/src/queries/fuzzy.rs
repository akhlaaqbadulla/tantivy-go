//! Typo tolerance by TERM EXPANSION rather than by automaton scoring.
//!
//! # Why not `tantivy::query::FuzzyTermQuery`
//!
//! Tantivy ships one. Its `Query::weight` ignores the `EnableScoring` it is
//! handed and always returns an `AutomatonWeight`, whose scorer is a
//! `ConstScorer`: every document matching the automaton receives the same
//! score. Used on its own that is fine for a filter, but it is fatal inside a
//! ranked boolean query — "dismissal" and a rare hapax one edit away from it
//! contribute exactly the same weight, and the BM25 signal the rest of the
//! query depends on is gone.
//!
//! So instead of asking the automaton to score, this module asks it only to
//! ENUMERATE. Each analyzed query token is streamed against every segment's
//! term dictionary through a Levenshtein DFA, the matching terms are collected,
//! and the result is an ordinary `BooleanQuery` of ordinary `TermQuery`s. BM25
//! applies unchanged, and each alternative carries a boost that reflects how
//! far it is from what the user typed.
//!
//! This is the shape Meilisearch's typo tolerance has: a Levenshtein automaton
//! intersected with the FST of known terms, with the edit budget gated on word
//! length so that short words are not allowed to match everything.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder, DFA};
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{Index, Score, Term};
use tantivy_fst::Automaton;

use crate::tantivy_util::TantivyGoError;

/// Ceiling on the terms one token may expand to, summed over all segments.
///
/// An unbounded expansion is a denial-of-service in one query: a three-letter
/// token at distance 2 matches a large fraction of any real dictionary, and
/// every match becomes a posting list to open. 50 is Lucene's default for
/// `max_expansions` and is generous for the job — typo tolerance needs the
/// handful of near-spellings, not the whole neighbourhood.
const MAX_FUZZY_EXPANSIONS: usize = 50;

/// Ceiling on the completions one prefix may expand to. Same reasoning and
/// same number as the fuzzy cap: enough to cover what a half-typed word can
/// reasonably mean, bounded so a two-character prefix cannot open thousands of
/// posting lists.
const MAX_PREFIX_EXPANSIONS: usize = 50;

/// Shortest prefix worth expanding. Below this the completion set stops
/// carrying information — and the planner already refuses shorter tokens, so
/// this is the engine-side backstop rather than the policy.
const MIN_PREFIX_LEN: usize = 3;

/// Length gates, in characters, for each edit budget.
///
/// Meilisearch's thresholds, and the reasoning is the same: one edit on a
/// four-letter word reaches most other four-letter words, so the match stops
/// meaning anything. Below `MIN_LEN_D1` a token is matched exactly.
const MIN_LEN_D1: usize = 5;
const MIN_LEN_D2: usize = 9;

/// Boost applied to an alternative by its edit distance from the query token.
///
/// Index 0 is the exact term. The decay is deliberately steep: a fuzzy match is
/// a guess about what the user meant, and it should only win when nothing
/// closer matched at all.
const DISTANCE_BOOST: [f32; 3] = [1.0, 0.5, 0.25];

/// Wraps a Levenshtein DFA as an FST automaton.
///
/// Tantivy has exactly this type but keeps it `pub(crate)`, so it is
/// re-implemented here rather than reached for.
struct DfaWrapper(DFA);

impl Automaton for DfaWrapper {
    type State = u32;

    fn start(&self) -> Self::State {
        self.0.initial_state()
    }

    fn is_match(&self, state: &Self::State) -> bool {
        matches!(self.0.distance(*state), Distance::Exact(_))
    }

    fn can_match(&self, state: &u32) -> bool {
        *state != levenshtein_automata::SINK_STATE
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        self.0.transition(*state, byte)
    }
}

/// Matches every term beginning with a byte prefix.
///
/// Written here rather than pulled from a crate: `tantivy-fst` 0.5 exposes
/// `StartsWith<A>` but no string automaton to wrap, and the whole thing is
/// fifteen lines. `can_match` returning false is what lets the FST prune a
/// subtree instead of walking the whole dictionary.
struct PrefixAutomaton<'a>(&'a [u8]);

impl Automaton for PrefixAutomaton<'_> {
    /// How many prefix bytes have matched, or None once the branch is dead.
    type State = Option<usize>;

    fn start(&self) -> Self::State {
        Some(0)
    }

    fn is_match(&self, state: &Self::State) -> bool {
        matches!(state, Some(i) if *i >= self.0.len())
    }

    fn can_match(&self, state: &Self::State) -> bool {
        state.is_some()
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        match state {
            // Past the prefix: everything that follows is part of the match.
            Some(i) if *i >= self.0.len() => Some(*i),
            Some(i) if self.0[*i] == byte => Some(i + 1),
            _ => None,
        }
    }
}

/// Builders are cached because constructing one instantiates a parametric DFA,
/// which is orders of magnitude more expensive than deriving a DFA for a
/// specific word from it. Indexed `[distance][transposition]`.
fn automaton_builder(distance: u8, transposition: bool) -> &'static LevenshteinAutomatonBuilder {
    static BUILDERS: [[OnceLock<LevenshteinAutomatonBuilder>; 2]; 3] = [
        [OnceLock::new(), OnceLock::new()],
        [OnceLock::new(), OnceLock::new()],
        [OnceLock::new(), OnceLock::new()],
    ];
    BUILDERS[distance as usize][transposition as usize]
        .get_or_init(|| LevenshteinAutomatonBuilder::new(distance, transposition))
}

/// How many edits this token is actually allowed, given what was requested.
///
/// Two rules narrow the request, and both exist because the alternative was
/// measured to be worse than no typo tolerance at all:
///
///   * A token containing a digit is never fuzzed. In a legal corpus those are
///     section numbers, years and citations — "64" is one edit from "46", "65"
///     and "14", so fuzzing them turns a pinpoint lookup into a scattergun.
///   * Short tokens get a smaller budget, because one edit on a short word
///     reaches too much of the dictionary to carry information.
fn effective_distance(token: &str, requested: u8) -> u8 {
    if requested == 0 {
        return 0;
    }
    if token.chars().any(|c| c.is_ascii_digit()) {
        return 0;
    }
    let len = token.chars().count();
    let allowed = if len >= MIN_LEN_D2 {
        2
    } else if len >= MIN_LEN_D1 {
        1
    } else {
        0
    };
    requested.min(allowed).min(2)
}

/// Collects every term an automaton matches, across every segment, and keeps
/// the lexicographically smallest `cap` of them.
///
/// # Why a BTreeSet and not a cap checked while streaming
///
/// The obvious loop stops as soon as it has `cap` terms. That is not
/// deterministic here: each `themis-search` replica builds its own index from
/// the outbox, so the two have different SEGMENT layouts, and a stream that
/// stops early takes whatever the first segments happened to offer. Two
/// replicas then answer the same query with different terms — the exact class
/// of divergence a chunk-id tiebreak was added to the service to remove.
///
/// Keeping the smallest `cap` instead is independent of segment layout,
/// because each segment's stream is itself ordered and the smallest N of a
/// union is well defined. For a prefix that also happens to be the ordering
/// worth having: the shortest completions sort first.
fn collect_terms<A, F>(
    searcher: &tantivy::Searcher,
    field: Field,
    make_automaton: F,
    cap: usize,
) -> Result<BTreeSet<String>, TantivyGoError>
where
    A: Automaton,
    // `TermDictionary::search` requires it, and both automata here satisfy it
    // (a DFA state is a u32, a prefix state an Option<usize>).
    A::State: Clone,
    F: Fn() -> A,
{
    let mut kept: BTreeSet<String> = BTreeSet::new();
    for segment in searcher.segment_readers() {
        let inverted = match segment.inverted_index(field) {
            Ok(inv) => inv,
            // A segment that has never seen this field has nothing to
            // contribute; that is not an error for the query as a whole.
            Err(_) => continue,
        };
        let mut stream = inverted
            .terms()
            .search(make_automaton())
            .into_stream()
            .map_err(|e| TantivyGoError(format!("Cannot stream term dictionary: {e}")))?;
        while stream.advance() {
            let Ok(text) = std::str::from_utf8(stream.key()) else {
                continue;
            };
            // Past the cap, a term only earns a place by displacing a larger
            // one — so the set holds the smallest `cap` seen so far and the
            // result cannot depend on which segment was read first.
            if kept.len() >= cap {
                match kept.last() {
                    Some(largest) if text >= largest.as_str() => continue,
                    _ => {
                        kept.pop_last();
                    }
                }
            }
            kept.insert(text.to_string());
        }
    }
    Ok(kept)
}

/// Returns `(distance, term_text)` for every indexed term within `distance`
/// edits of `token`, excluding `token` itself.
///
/// Distances are assigned by running the narrowest automaton first: a term
/// found by the distance-1 pass is at distance 1, and the distance-2 pass then
/// only contributes terms it did not already find. That is cheaper than
/// recomputing an edit distance per term and cannot disagree with the
/// automaton that actually matched. The budget is spent in that order too, so
/// a near miss is never displaced by a far one.
fn expand_token(
    index: &Index,
    field: Field,
    token: &str,
    distance: u8,
    transposition: bool,
    prefix: bool,
) -> Result<Vec<(u8, String)>, TantivyGoError> {
    if distance == 0 {
        return Ok(Vec::new());
    }
    let reader = index
        .reader()
        .map_err(|e| TantivyGoError(format!("Cannot open reader for fuzzy expansion: {e}")))?;
    let searcher = reader.searcher();

    let mut out: Vec<(u8, String)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    seen.insert(token.to_string());

    for d in 1..=distance {
        let remaining = MAX_FUZZY_EXPANSIONS.saturating_sub(out.len());
        if remaining == 0 {
            break;
        }
        let builder = automaton_builder(d, transposition);
        let found = collect_terms(
            &searcher,
            field,
            || {
                let dfa = if prefix {
                    builder.build_prefix_dfa(token)
                } else {
                    builder.build_dfa(token)
                };
                DfaWrapper(dfa)
            },
            // Over-collect so that terms already taken at a smaller distance
            // do not eat this pass's share.
            remaining + seen.len(),
        )?;
        for text in found {
            if out.len() >= MAX_FUZZY_EXPANSIONS {
                break;
            }
            if seen.insert(text.clone()) {
                out.push((d, text));
            }
        }
    }
    Ok(out)
}

/// Returns every indexed term beginning with `token`, excluding `token`
/// itself, capped and deterministic.
fn expand_prefix(
    index: &Index,
    field: Field,
    token: &str,
) -> Result<Vec<String>, TantivyGoError> {
    let reader = index
        .reader()
        .map_err(|e| TantivyGoError(format!("Cannot open reader for prefix expansion: {e}")))?;
    let searcher = reader.searcher();
    let bytes = token.as_bytes();
    let found = collect_terms(
        &searcher,
        field,
        || PrefixAutomaton(bytes),
        MAX_PREFIX_EXPANSIONS + 1,
    )?;
    Ok(found.into_iter().filter(|t| t != token).take(MAX_PREFIX_EXPANSIONS).collect())
}

/// Builds the alternatives for one analyzed token matched as a PREFIX.
///
/// # Why this exists rather than `PhrasePrefixQuery`
///
/// That is what a prefix clause used to become, and it is const-scored: a
/// production probe of `judicia` returned exactly TWO distinct scores — one
/// real BM25 hit and 199 documents tied at the clause boost. Two things follow
/// from a tie that large. The clause contributes reach but no ranking, and in
/// chunk mode, where the fetch equals the limit, the tie group is cut inside
/// Tantivy's top-K collector before any deterministic sort runs — so two
/// replicas returned 199 of 200 different chunks.
///
/// Expanding to real `TermQuery`s fixes both at once: BM25 scores each
/// completion on its own IDF, so a rare one outranks a common one and the tie
/// disappears. Every expansion carries boost 1.0 — unlike the fuzzy ladder,
/// where edit distance is a real signal, prefix LENGTH is not: `judicia` as a
/// literal token is no more what the user meant than `judicial`. Lucene's
/// scoring prefix rewrite does the same.
pub fn prefix_alternatives(
    index: &Index,
    field: Field,
    term: Term,
) -> Result<Box<dyn Query>, TantivyGoError> {
    let exact = Box::new(TermQuery::new(term.clone(), IndexRecordOption::WithFreqs));
    let Some(token) = term.value().as_str().map(|s| s.to_string()) else {
        return Ok(exact);
    };
    if token.chars().count() < MIN_PREFIX_LEN {
        return Ok(exact);
    }
    let expansions = expand_prefix(index, field, &token)?;
    if expansions.is_empty() {
        return Ok(exact);
    }
    let mut subs: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(expansions.len() + 1);
    subs.push((Occur::Should, exact));
    for text in expansions {
        subs.push((
            Occur::Should,
            Box::new(TermQuery::new(
                Term::from_field_text(field, &text),
                IndexRecordOption::WithFreqs,
            )),
        ));
    }
    Ok(Box::new(BooleanQuery::new(subs)))
}

/// Builds the alternatives for one analyzed token: the exact term at full
/// weight, plus each expansion boosted down by its edit distance.
///
/// Returns a bare `TermQuery` when nothing expanded, so a query with no typos
/// in it is byte-identical to the non-fuzzy one.
pub fn token_alternatives(
    index: &Index,
    field: Field,
    term: Term,
    requested_distance: u8,
    transposition: bool,
    prefix: bool,
) -> Result<Box<dyn Query>, TantivyGoError> {
    let exact = Box::new(TermQuery::new(term.clone(), IndexRecordOption::WithFreqs));
    let Some(token) = term.value().as_str().map(|s| s.to_string()) else {
        // A non-string term cannot be fuzzed; match it exactly.
        return Ok(exact);
    };
    let distance = effective_distance(&token, requested_distance);
    let expansions = expand_token(index, field, &token, distance, transposition, prefix)?;
    if expansions.is_empty() {
        return Ok(exact);
    }
    let mut subs: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(expansions.len() + 1);
    subs.push((Occur::Should, exact));
    for (d, text) in expansions {
        let alt = Box::new(TermQuery::new(
            Term::from_field_text(field, &text),
            IndexRecordOption::WithFreqs,
        ));
        let boost = DISTANCE_BOOST[d as usize] as Score;
        subs.push((Occur::Should, Box::new(BoostQuery::new(alt, boost))));
    }
    Ok(Box::new(BooleanQuery::new(subs)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_tokens_are_not_fuzzed() {
        assert_eq!(effective_distance("act", 2), 0);
        assert_eq!(effective_distance("work", 1), 0);
    }

    #[test]
    fn medium_tokens_get_one_edit() {
        assert_eq!(effective_distance("wages", 2), 1);
        assert_eq!(effective_distance("notice", 2), 1);
    }

    #[test]
    fn long_tokens_get_the_full_budget() {
        assert_eq!(effective_distance("dismissal", 2), 2);
        assert_eq!(effective_distance("termination", 2), 2);
        // A request for less is still honoured.
        assert_eq!(effective_distance("termination", 1), 1);
    }

    #[test]
    fn tokens_carrying_a_digit_are_never_fuzzed() {
        // The whole point: s.64 must not match s.46.
        assert_eq!(effective_distance("64", 2), 0);
        assert_eq!(effective_distance("2019", 2), 0);
        assert_eq!(effective_distance("section64", 2), 0);
    }

    #[test]
    fn zero_request_stays_zero() {
        assert_eq!(effective_distance("termination", 0), 0);
    }

    // --- Integration: the expansion actually runs against an index. ---
    //
    // The gate tests above are arithmetic. These are the ones that prove the
    // feature works, because every way this can fail silently — an automaton
    // that matches nothing, a term dictionary that is never streamed, a
    // ConstScorer flattening the ranking — leaves `effective_distance` right
    // and the search wrong.

    use crate::queries::convert::convert_to_tantivy;
    use crate::queries::models::BoolQuery;
    use crate::queries::{FinalQuery, GoQuery, QueryElement, QueryModifier};
    use tantivy::collector::TopDocs;
    use tantivy::schema::{IndexRecordOption, Schema, TextFieldIndexing, Value, STORED, TEXT};
    use tantivy::tokenizer::{SimpleTokenizer, TextAnalyzer};
    use tantivy::{doc, Index, IndexWriter, TantivyDocument};

    fn indexed(bodies: &[&str]) -> (Index, Schema) {
        let mut b = Schema::builder();
        let opts = (TEXT | STORED).set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("simple")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let body = b.add_text_field("body", opts);
        let schema = b.build();
        let index = Index::create_in_ram(schema.clone());
        index.tokenizers().register(
            "simple",
            TextAnalyzer::builder(SimpleTokenizer::default()).build(),
        );
        let mut w: IndexWriter = index.writer(15_000_000).unwrap();
        for text in bodies {
            w.add_document(doc!(body => *text)).unwrap();
        }
        w.commit().unwrap();
        (index, schema)
    }

    fn one_clause(query: GoQuery) -> FinalQuery {
        FinalQuery {
            texts: vec!["dismisal".to_string()],
            fields: vec!["body".to_string()],
            query: BoolQuery {
                subqueries: vec![QueryElement {
                    query: Some(query),
                    modifier: QueryModifier::Must,
                }],
            },
        }
    }

    fn run(index: &Index, schema: &Schema, fq: FinalQuery) -> Vec<(f32, String)> {
        run_n(index, schema, fq, 10)
    }

    /// The prefix tests need more than the default ten slots: their whole
    /// point is how a LARGE result set is scored and ordered, and a limit of
    /// ten would cut the evidence off before the assertion sees it.
    fn run_n(index: &Index, schema: &Schema, fq: FinalQuery, limit: usize) -> Vec<(f32, String)> {
        let q = convert_to_tantivy(index, fq, schema).expect("conversion failed");
        let searcher = index.reader().unwrap().searcher();
        let body = schema.get_field("body").unwrap();
        searcher
            .search(&q, &TopDocs::with_limit(limit))
            .unwrap()
            .into_iter()
            .map(|(score, addr)| {
                let d: TantivyDocument = searcher.doc(addr).unwrap();
                let text = d
                    .get_first(body)
                    .and_then(|v| v.as_str().map(|s| s.to_string()))
                    .unwrap_or_default();
                (score, text)
            })
            .collect()
    }

    #[test]
    fn a_typo_finds_nothing_without_fuzzy_and_the_right_thing_with_it() {
        let (index, schema) = indexed(&[
            "dismissal of an employee",
            "termination of employment",
            "wages and salary",
        ]);

        // The control. "dismisal" is not a term in this index, so the strict
        // clause has nothing to match. This is the bug the user reported.
        let strict = run(
            &index,
            &schema,
            one_clause(GoQuery::OneOfTermQuery {
                field_index: 0,
                text_index: 0,
                boost: 1.0,
            }),
        );
        assert!(
            strict.is_empty(),
            "strict search should find nothing, found {strict:?}"
        );

        // The same query, typo-tolerant. "dismisal" is 8 characters, so it
        // gets one edit, and "dismissal" is exactly one insertion away.
        let fuzzy = run(
            &index,
            &schema,
            one_clause(GoQuery::OneOfFuzzyTermQuery {
                field_index: 0,
                text_index: 0,
                distance: 1,
                transposition: true,
                prefix: false,
                boost: 1.0,
            }),
        );
        assert_eq!(fuzzy.len(), 1, "fuzzy search returned {fuzzy:?}");
        assert!(fuzzy[0].1.contains("dismissal"));
        assert!(fuzzy[0].0 > 0.0, "a fuzzy hit must carry a real score");
    }

    #[test]
    fn an_exact_match_outranks_a_fuzzy_one() {
        // The reason this module expands terms instead of using tantivy's
        // FuzzyTermQuery: that one's scorer is constant, so these two
        // documents would tie and the ordering would be arbitrary.
        let (index, schema) = indexed(&["dismisal dismisal dismisal", "dismissal"]);
        let hits = run(
            &index,
            &schema,
            one_clause(GoQuery::OneOfFuzzyTermQuery {
                field_index: 0,
                text_index: 0,
                distance: 1,
                transposition: true,
                prefix: false,
                boost: 1.0,
            }),
        );
        assert_eq!(hits.len(), 2, "expected both documents, got {hits:?}");
        assert!(
            hits[0].1.starts_with("dismisal"),
            "the exact match must rank first, got {hits:?}"
        );
        assert!(
            hits[0].0 > hits[1].0,
            "scores must differ; a ConstScorer would tie them: {hits:?}"
        );
    }

    // ── Prefix matching. ──────────────────────────────────────────────────
    //
    // The bug these cover: a prefix clause used to become
    // `PhrasePrefixQuery`, which is const-scored. A production probe of
    // `judicia` returned exactly TWO distinct scores — one real hit and 199
    // documents tied at the clause boost.

    fn indexed_batches(batches: &[&[&str]]) -> (Index, Schema) {
        let mut b = Schema::builder();
        let opts = (TEXT | STORED).set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("simple")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
        let body = b.add_text_field("body", opts);
        let schema = b.build();
        let index = Index::create_in_ram(schema.clone());
        index.tokenizers().register(
            "simple",
            TextAnalyzer::builder(SimpleTokenizer::default()).build(),
        );
        let mut w: IndexWriter = index.writer(15_000_000).unwrap();
        // One commit per batch, so each batch becomes its own SEGMENT.
        for batch in batches {
            for text in *batch {
                w.add_document(doc!(body => *text)).unwrap();
            }
            w.commit().unwrap();
        }
        (index, schema)
    }

    fn prefix_clause(text_index: usize) -> GoQuery {
        GoQuery::PrefixTermQuery {
            field_index: 0,
            text_index,
            boost: 1.0,
        }
    }

    #[test]
    fn a_prefix_matches_its_completions() {
        let (index, schema) = indexed_batches(&[&[
            "judicial review of the decision",
            "the judiciary is independent",
            "wages are payable monthly",
        ]]);
        let mut fq = one_clause(prefix_clause(0));
        fq.texts = vec!["judicia".to_string()];
        let hits = run(&index, &schema, fq);
        assert_eq!(hits.len(), 2, "expected both completions, got {hits:?}");
    }

    #[test]
    fn a_prefix_below_the_minimum_length_does_not_expand() {
        // The engine-side backstop. The planner already refuses tokens under
        // three characters, but a two-character prefix opens a large fraction
        // of any real dictionary and nothing downstream would notice.
        let (index, schema) = indexed_batches(&[&["zzcommon filler", "zzother filler"]]);

        let mut fq = one_clause(prefix_clause(0));
        fq.texts = vec!["zz".to_string()];
        assert!(
            run(&index, &schema, fq).is_empty(),
            "a two-character prefix expanded; MIN_PREFIX_LEN is not being applied"
        );

        // Three characters is allowed, and reaches the one term it prefixes.
        let mut fq = one_clause(prefix_clause(0));
        fq.texts = vec!["zzc".to_string()];
        assert_eq!(run(&index, &schema, fq).len(), 1);
    }

    #[test]
    fn a_rarer_completion_outranks_a_common_one() {
        // One document carries both completions, so document length and term
        // frequency are held equal and only IDF can separate them.
        let mut docs: Vec<String> = vec!["prefixrare prefixcommon".to_string()];
        for _ in 0..30 {
            docs.push("prefixcommon filler".to_string());
        }
        let refs: Vec<&str> = docs.iter().map(String::as_str).collect();
        let (index, schema) = indexed_batches(&[&refs]);

        let mut fq = one_clause(prefix_clause(0));
        fq.texts = vec!["prefix".to_string()];
        let hits = run_n(&index, &schema, fq, 100);
        assert_eq!(hits.len(), 31, "every document has a completion");
        let scores: std::collections::BTreeSet<String> =
            hits.iter().map(|(s, _)| format!("{s:.4}")).collect();
        assert!(
            scores.len() > 1,
            "every completion scored the same — this is the ConstScorer bug: {hits:?}"
        );
        // The document holding the RARE completion must come first.
        assert!(
            hits[0].1.starts_with("prefixrare"),
            "the rarer completion did not rank first: {hits:?}"
        );
    }

    #[test]
    fn expansion_does_not_depend_on_segment_layout() {
        // Two replicas of themis-search build their own index from the same
        // outbox, so they have DIFFERENT segment layouts. A cap applied while
        // streaming takes whatever the first segments offered, and the two
        // answer the same query with different terms. The collector keeps the
        // lexicographically smallest N instead, which a segment split cannot
        // change.
        let mut words: Vec<String> = (0..80).map(|i| format!("prefixed{i:03} filler")).collect();
        words.sort();
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();

        let (a, sa) = indexed_batches(&[&refs]);
        let (b, sb) = indexed_batches(&[&refs[40..], &refs[..40]]);
        let (c, sc) = indexed_batches(&[&refs[..10], &refs[10..55], &refs[55..]]);

        let ids = |index: &Index, schema: &Schema| -> Vec<String> {
            let mut fq = one_clause(prefix_clause(0));
            fq.texts = vec!["prefixed".to_string()];
            let mut out: Vec<String> =
                run_n(index, schema, fq, 200).into_iter().map(|(_, t)| t).collect();
            out.sort();
            out
        };
        let one = ids(&a, &sa);
        // 80 terms match, the cap is 50, and `prefixed` is not itself an
        // indexed term — so the exact clause contributes nothing and the
        // result is exactly the cap. If this ever equals 80 the cap stopped
        // biting and the test below proves nothing.
        assert_eq!(
            one.len(),
            MAX_PREFIX_EXPANSIONS,
            "the cap must bite for this test to mean anything"
        );
        assert_eq!(one, ids(&b, &sb), "a different segment layout changed the expansion");
        assert_eq!(one, ids(&c, &sc), "a third segment layout changed the expansion");
    }

    #[test]
    fn a_numeric_token_is_not_expanded() {
        // s.64 must never be answered with s.46. The gate is arithmetic, but
        // this proves it is actually consulted on the query path.
        let (index, schema) = indexed(&["section 46 applies", "section 65 applies"]);
        let mut fq = one_clause(GoQuery::OneOfFuzzyTermQuery {
            field_index: 0,
            text_index: 0,
            distance: 2,
            transposition: true,
            prefix: false,
            boost: 1.0,
        });
        fq.texts = vec!["64".to_string()];
        let hits = run(&index, &schema, fq);
        assert!(hits.is_empty(), "64 must not reach 46 or 65, got {hits:?}");
    }
}
