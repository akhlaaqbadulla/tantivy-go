package tantivy_go

type QueryType int

const (
	BoolQuery QueryType = iota
	PhraseQuery
	PhrasePrefixQuery
	TermPrefixQuery
	TermQuery
	EveryTermQuery
	OneOfTermQuery
	AllQuery
	// FuzzyTermQuery matches a single analyzed token typo-tolerantly by
	// expanding a Levenshtein automaton against the term dictionary into
	// ordinary term queries. It is NOT tantivy's own FuzzyTermQuery, whose
	// scorer is constant and therefore unusable inside a ranked query.
	FuzzyTermQuery
	// OneOfFuzzyTermQuery is the multi-token form: every analyzed token of
	// the text gets its own exact term plus fuzzy alternatives, nested inside
	// that token's positional weight.
	OneOfFuzzyTermQuery
)

// Fuzzy defaults. Distance is capped at 2 by the Levenshtein automaton, and
// further capped per token by its length on the Rust side.
const (
	DefaultFuzzyDistance = 1
	MaxFuzzyDistance     = 2
)

type QueryModifier int

const (
	Must QueryModifier = iota
	Should
	MustNot
)

type FieldQuery struct {
	FieldIndex int     `json:"field_index"`
	TextIndex  int     `json:"text_index"`
	Boost      float64 `json:"boost"`
}

// FuzzyFieldQuery carries the three parameters a plain FieldQuery has no room
// for. The Rust side defaults every one of them, so an older library reading a
// newer payload degrades to distance 1 rather than failing.
type FuzzyFieldQuery struct {
	FieldIndex    int     `json:"field_index"`
	TextIndex     int     `json:"text_index"`
	Distance      uint8   `json:"distance"`
	Transposition bool    `json:"transposition"`
	Prefix        bool    `json:"prefix"`
	Boost         float64 `json:"boost"`
}

type QueryElement struct {
	Query     Query         `json:"query"`
	Modifier  QueryModifier `json:"query_modifier"`
	QueryType QueryType     `json:"query_type"`
}

type BooleanQuery struct {
	Subqueries []QueryElement `json:"subqueries"`
	Boost      float64        `json:"boost"`
}

type FinalQuery struct {
	Texts  []string      `json:"texts"`
	Fields []string      `json:"fields"`
	Query  *BooleanQuery `json:"query"`
}

type sharedStore struct {
	texts     map[string]int
	fields    map[string]int
	textList  []string
	fieldList []string
}

type QueryBuilder struct {
	store      *sharedStore
	subqueries []QueryElement
}

func NewQueryBuilder() *QueryBuilder {
	return &QueryBuilder{
		store: &sharedStore{
			texts:     make(map[string]int),
			fields:    make(map[string]int),
			textList:  []string{},
			fieldList: []string{},
		},
		subqueries: []QueryElement{},
	}
}

func (qb *QueryBuilder) NestedBuilder() *QueryBuilder {
	return &QueryBuilder{
		store:      qb.store,
		subqueries: []QueryElement{},
	}
}

func (qb *QueryBuilder) AddText(text string) int {
	if idx, exists := qb.store.texts[text]; exists {
		return idx
	}
	idx := len(qb.store.textList)
	qb.store.texts[text] = idx
	qb.store.textList = append(qb.store.textList, text)
	return idx
}

func (qb *QueryBuilder) AddField(field string) int {
	if idx, exists := qb.store.fields[field]; exists {
		return idx
	}
	idx := len(qb.store.fieldList)
	qb.store.fields[field] = idx
	qb.store.fieldList = append(qb.store.fieldList, field)
	return idx
}

func (qb *QueryBuilder) Query(modifier QueryModifier, field string, text string, queryType QueryType, boost float64) *QueryBuilder {
	textIndex := qb.AddText(text)
	fieldIndex := qb.AddField(field)
	qb.subqueries = append(qb.subqueries, QueryElement{
		Query: &FieldQuery{
			FieldIndex: fieldIndex,
			TextIndex:  textIndex,
			Boost:      boost,
		},
		Modifier:  modifier,
		QueryType: queryType,
	})
	return qb
}

// FuzzyQuery adds a typo-tolerant clause.
//
// distance is the maximum edit distance (0..=2); transposition enables
// Damerau-Levenshtein, where a swap of adjacent characters costs one edit
// rather than two; prefix matches an indexed term that merely STARTS with
// something within distance of text, which is what search-as-you-type needs.
//
// queryType must be FuzzyTermQuery (first analyzed token only) or
// OneOfFuzzyTermQuery (every analyzed token).
func (qb *QueryBuilder) FuzzyQuery(
	modifier QueryModifier,
	field string,
	text string,
	queryType QueryType,
	distance uint8,
	transposition bool,
	prefix bool,
	boost float64,
) *QueryBuilder {
	if distance > MaxFuzzyDistance {
		distance = MaxFuzzyDistance
	}
	textIndex := qb.AddText(text)
	fieldIndex := qb.AddField(field)
	qb.subqueries = append(qb.subqueries, QueryElement{
		Query: &FuzzyFieldQuery{
			FieldIndex:    fieldIndex,
			TextIndex:     textIndex,
			Distance:      distance,
			Transposition: transposition,
			Prefix:        prefix,
			Boost:         boost,
		},
		Modifier:  modifier,
		QueryType: queryType,
	})
	return qb
}

func (qb *QueryBuilder) BooleanQuery(modifier QueryModifier, subBuilder *QueryBuilder, boost float64) *QueryBuilder {
	qb.subqueries = append(qb.subqueries, QueryElement{
		Query: &BooleanQuery{
			Subqueries: subBuilder.subqueries,
			Boost:      boost,
		},
		Modifier:  modifier,
		QueryType: BoolQuery,
	})
	return qb
}

func (qb *QueryBuilder) AllQuery(modifier QueryModifier, boost float64) *QueryBuilder {
	qb.subqueries = append(qb.subqueries, QueryElement{
		Query:     &AllQueryStruct{Boost: boost},
		Modifier:  modifier,
		QueryType: AllQuery,
	})
	return qb
}

func (qb *QueryBuilder) Build() FinalQuery {
	return FinalQuery{
		Texts:  qb.store.textList,
		Fields: qb.store.fieldList,
		Query: &BooleanQuery{
			Subqueries: qb.subqueries,
		},
	}
}

type Query interface {
	IsQuery()
}

func (fq *FieldQuery) IsQuery() {}

func (fq *FuzzyFieldQuery) IsQuery() {}

func (bq *BooleanQuery) IsQuery() {}

type AllQueryStruct struct {
	Boost float64 `json:"boost"`
}

func (aq *AllQueryStruct) IsQuery() {}
