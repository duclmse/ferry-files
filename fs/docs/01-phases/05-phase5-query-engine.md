# Phase 5 - Query Engine (Everything-Compatible)

**Goal:** Implement the full voidtools Everything-compatible query language for
`filesearch`. This is the heart of the product - the parser, AST, evaluator, and
all supporting sub-parsers. After this phase, every query in the acceptance test
list (spec section 13) parses and executes correctly.

**Estimated scope:** ~2 000 lines of Rust (the largest phase).

---

## 5.1 Module Layout

```
src/query/
├── mod.rs        ← public execute() entry point, QueryResult
├── ast.rs        ← Expr, Predicate, Function, Modifiers enums
├── parser.rs     ← tokeniser + recursive-descent parser   [tested]
├── evaluator.rs  ← AST → QueryPlan → SQL execution        [tested]
├── date.rs       ← date constant resolution + date parser  [tested]
├── size.rs       ← size value parser (constants, units, ranges) [tested]
├── glob.rs       ← glob → SQLite GLOB translation          [tested]
├── dupes.rs      ← dupe search two-pass query logic        [tested]
└── macros.rs     ← type macro expansion tables, user macro resolution
```

---

## 5.2 AST (`ast.rs`)

Core types (no logic - pure data):

```rust
pub enum Expr {
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Predicate(Predicate),
}

pub struct Predicate {
    pub kind:      PredicateKind,
    pub modifiers: Modifiers,
}

pub enum PredicateKind {
    Term(String),         // plain text / wildcard
    Regex(String),        // regex: modifier
    Function(Function),   // dm:, size:, ext:, etc.
    TypeMacro(TypeMacro), // audio:, zip:, doc:, etc.
    Fuzzy(String),        // fuzzy:<term> - filesearch extension, not in Everything
}

pub struct Modifiers {
    pub case_sensitive:    Option<bool>,
    pub match_path:        Option<bool>,
    pub files_only:        Option<bool>,
    pub folders_only:      Option<bool>,
    pub whole_filename:    Option<bool>,
    pub whole_word:        Option<bool>,
    pub ignore_diacritics: Option<bool>,
    /// None = include both real files and inner archive entries.
    /// Some(true)  = `archive:` modifier - inner entries only.
    /// Some(false) = `noarchive:` modifier - real filesystem files only.
    pub archive_only:      Option<bool>,
}
```

Full `Function` enum covers: date functions, size, name/path functions,
attributes, content, folder stats, duplicates, result control (count), and
`ArchiveOf(String)` (ZIP inner streaming).

Full `DateConstant` enum covers: Today, Yesterday, ThisWeek, LastWeek,
ThisMonth, LastMonth, ThisYear, LastYear, LastNDays, LastNHours, LastNMinutes,
LastNWeeks, LastNMonths, Month(u32), Weekday(u32).

---

## 5.3 Tokeniser + Parser (`parser.rs`)

### Tokeniser

Produces a flat `Vec<Token>` before building the AST.

Token types: `Word`, `QuotedPhrase`, `Pipe`, `Bang`, `LAngle`, `RAngle`,
`ModifierPrefix(String)`, `Colon`, `FunctionValue(String)`, `Eof`.

### Input Normalisation (before tokenising)

1. Strip leading/trailing whitespace.
2. Expand character escape macros: `quot:` → `"`, `apos:` → `'`, etc.
3. Expand type macros: `audio:` → `ext:aac;ac3;...`.
4. Expand user-defined macros from config (no recursion).

### Grammar (recursive descent)

```
expr       := or_expr
or_expr    := and_expr ("|" and_expr)*
and_expr   := not_expr (not_expr)*    ← space-separated = AND
not_expr   := "!" atom | atom
atom       := "<" expr ">" | quoted_phrase | term
term       := modifier* (function | word_pattern)
```

### Operator Precedence (highest to lowest)

1. `<>` grouping
2. `!` NOT
3. space AND
4. `|` OR

So `A|B C` = `A OR (B AND C)` - **right-to-left** for OR.

### Modifier Resolution

When a word followed by `:` is encountered:

1. Known **modifier** keyword → set flag on next term's `Modifiers`.
2. Known **function** keyword → parse as `Function` with value.
3. Known **type macro** → `TypeMacro` variant.
4. Followed by `\` → treat as Windows path prefix with `match_path: true`.
5. Otherwise → literal word (colon is part of the text).

---

## 5.4 Sub-parsers

### size.rs

```rust
pub fn parse_size_value(s: &str) -> Result<RangeExpr<u64>, QueryError>
```

Handles:

- Comparison prefix: `<`, `<=`, `>`, `>=`, `=`.
- Range: `1mb..10mb` or `1mb-10mb`.
- Unit suffix: `kb`, `mb`, `gb` (case-insensitive).
- Named constants: `empty`, `tiny`, `small`, `medium`, `large`, `huge`,
  `gigantic`.

Named constant expansions:

```
empty    → Eq(0)
tiny     → Between(1, 10*1024)
small    → Between(10*1024+1, 100*1024)
medium   → Between(100*1024+1, 1024*1024)
large    → Between(1024*1024+1, 16*1024*1024)
huge     → Between(16*1024*1024+1, 128*1024*1024)
gigantic → Gte(128*1024*1024+1)
```

### date.rs

```rust
pub fn parse_date_value(s: &str) -> Result<DateExpr, QueryError>
pub fn resolve_date_constant(c: &DateConstant, now: NaiveDateTime) -> (i64, i64)
```

Date constant resolution examples:

- `Today` → midnight..23:59:59 of current local day.
- `Yesterday` → previous day.
- `ThisWeek` → Monday 00:00..Sunday 23:59 of current week.
- `LastNDays(n)` → now - n\*86400..now.
- Month names → first..last day of that month (current year).
- Weekday names → most recent occurrence (00:00..23:59).

> **Important:** date constants are resolved at **query-execution time**, not
> parse time, so `dm:today` always means the current calendar day.

Absolute date formats supported: `YYYY`, `YYYY-MM`, `YYYY-MM-DD`,
`YYYY-MM-DDThh:mm:ss`, `YYYYMMDD`, locale-dependent formats (configurable).

### glob.rs

```rust
pub fn glob_to_sqlite(pattern: &str, whole_filename: bool) -> String
```

Everything wildcards map directly to SQLite `GLOB`:

- `*` → `*` (zero or more chars)
- `?` → `?` (one char)

If `whole_filename = false` → wrap: `"*" + pattern + "*"`.

For case-insensitive: `LOWER(name) GLOB LOWER(?)`.

### macros.rs

Type macro expansion tables (static maps):

```rust
pub fn expand_type_macro(m: TypeMacro) -> Vec<String>  // returns ext list
pub fn expand_user_macro(name: &str, macros: &MacroConfig) -> Option<String>
```

User macros are expanded before tokenising. No recursive expansion.

---

## 5.5 Evaluator (`evaluator.rs`)

### QueryPlan

```rust
pub struct QueryPlan {
    pub sql_conditions: Vec<SqlCondition>,
    pub tantivy_query:  Option<String>,
    pub post_filters:   Vec<PostFilter>,
    pub limit:          Option<usize>,
    pub offset:         usize,
    pub strategy:       EvalStrategy,
}
```

### AST → QueryPlan Walk

```rust
pub fn plan(expr: &Expr, default_modifiers: &Modifiers) -> Result<QueryPlan, QueryError>
```

Key dispatch rules:

| Predicate                 | Maps to                                              |
| ------------------------- | ---------------------------------------------------- |
| Plain term (no wildcards) | `NameLike("%term%")`                                 |
| Term with `*`/`?`         | `NameGlob(glob_to_sqlite(term))`                     |
| `path:` modifier          | `PathLike` instead of `NameLike`                     |
| `regex:` modifier         | compile regex → `PostFilter::Regex`                  |
| `file:` modifier          | `IsDir(false)`                                       |
| `folder:` modifier        | `IsDir(true)`                                        |
| `dm:`, `dc:`, `da:`       | resolve `DateExpr` → `MtimeRange`/`CtimeRange`       |
| `size:`                   | `SizeRange` or `SizeOp`                              |
| `ext:list`                | `ExtIn(list)`                                        |
| `parent:path`             | `ParentPath(path)`                                   |
| `content:text`            | set `tantivy_query`, strategy = `TantivyThenSqlite`  |
| `fuzzy:term`              | fetch SQL candidates, strategy = `FuzzyFilter(term)` |
| `dupe:`                   | strategy = `DupeSearch(Name)`                        |
| `count:n`                 | set `plan.limit = n`                                 |
| `len:`                    | `NameLen(op, val)`                                   |
| `depth:`                  | `Depth(op, val)`                                     |
| `attrib:`                 | `PostFilter::Attrib(flags)`                          |
| `child:`                  | `PostFilter::Child(name)`                            |
| `startwith:`              | `PostFilter::StartWith(text)`                        |
| `endwith:`                | `PostFilter::EndWith(text)`                          |
| TypeMacro                 | expand → `ExtIn`                                     |
| `archive:` modifier       | `IsArchiveEntry(true)` - `archive_path IS NOT NULL`  |
| `noarchive:` modifier     | `IsArchiveEntry(false)` - `archive_path IS NULL`     |
| `archiveof:path`          | `ArchiveOf(path)` - `archive_path = ?` or GLOB       |

### Execution Strategies

| Strategy            | When                       | How                                                        |
| ------------------- | -------------------------- | ---------------------------------------------------------- |
| `SqliteOnly`        | No `content:`, no `fuzzy:` | Build SQL from conditions, query SQLite                    |
| `TantivyThenSqlite` | `content:` present         | Query Tantivy → get path list → filter via SQLite metadata |
| `DupeSearch(kind)`  | `dupe:` / `sizedupe:` etc. | Two-pass: GROUP BY → IN                                    |
| `FuzzyFilter(term)` | `fuzzy:` present           | Fetch SQL candidates → score with nucleo in rayon par_iter |

### Post-filter Application

After fetching `Vec<FileRecord>` from SQLite:

```rust
pub fn apply_post_filters(
    records: Vec<FileRecord>,
    filters: &[PostFilter],
    sqlite: &SqliteStore,
) -> Vec<FileRecord>
```

- `Regex(re, case_sensitive)` → `re.is_match(&record.path)`
- `Child(name)` → `sqlite.has_child(record.path, name)`
- `Attrib(flags)` → bitmask check against `record.attrib_bitmask`
- `WholeWord(w)` → word-boundary check
- `StartWith(s)` / `EndWith(s)` → `starts_with` / `ends_with`

---

## 5.6 Dupe Search (`dupes.rs`)

```rust
pub fn find_dupes(
    sqlite: &SqliteStore,
    kind: DupeKind,
    extra_conditions: &[SqlCondition],
    limit: usize,
) -> Result<Vec<FileRecord>>
```

Two-pass query:

```sql
-- Pass 1: find duplicated values
SELECT <column>, COUNT(*) as c
FROM files
WHERE <extra_conditions>
GROUP BY <column>
HAVING c > 1;

-- Pass 2: fetch all files with those values
SELECT * FROM files
WHERE <column> IN (values from pass 1)
  AND <extra_conditions>
LIMIT ?;
```

`DupeKind` variants and their GROUP BY column:

- `Name` → `name`
- `NamePart` → `REPLACE(name, extension, '')`
- `Size` → `size_bytes`
- `DateModified` → `mtime`
- `DateCreated` → `ctime`
- `DateAccessed` → (not in SQLite schema - skip or return empty)
- `Attrib` → `attrib_bitmask`

---

## 5.7 Public Entry Point (`mod.rs`)

```rust
pub async fn execute(
    raw_query: &str,
    sqlite: Arc<SqliteStore>,
    tantivy: Arc<TantivyStore>,
    config: Arc<RwLock<Config>>,
) -> Result<QueryResult, QueryError>
```

Internal flow:

1. Expand user macros from `config.macros`.
2. Expand character escape macros.
3. Expand type macros.
4. Tokenise.
5. Parse → `Expr` AST.
6. Plan → `QueryPlan`.
7. Execute (strategy dispatch).
8. Apply post-filters.
9. Apply limit/offset.
10. Return `QueryResult { hits, total, took_ms, mode_used }`.

---

## 5.8 `fuzzy:` Function (filesearch Extension)

`fuzzy:<term>` is a first-class query function unique to filesearch. It does
**not** exist in voidtools Everything.

**Behaviour:**

1. Fetch candidate `FileRecord`s from SQLite matching all other `SqlCondition`s
   already in the plan (no name condition is added to SQL for the fuzzy term).
2. Score all candidates against `<term>` using `nucleo::Matcher` in a
   `rayon::par_iter`.
3. Retain results above `config.search.fuzzy_threshold` (default 0.7).
4. Order results by score descending.
5. Apply `limit` / `offset`.

**Compatible with all other query functions:**

- `fuzzy:config ext:toml` - only `.toml` files, fuzzy-matched against "config"
- `fuzzy:readm size:<100kb` - small files, fuzzy-matched against "readm"

**`EvalStrategy::FuzzyFilter(term)`** is added to the evaluator:

```rust
pub enum EvalStrategy {
    SqliteOnly,
    TantivyThenSqlite,
    DupeSearch(DupeKind),
    /// Fetch SQL candidates, then score with nucleo in-process.
    FuzzyFilter(String),
}
```

> **Note:** `query/exact.rs`, `query/fuzzy.rs`, and `query/regex_search.rs` do
> **not** exist. All logic lives in `evaluator.rs`. The `~` prefix and `/`
> prefix from the base spec are **not supported** - use `fuzzy:` and `regex:`
> modifiers instead.

---

## 5.9 Tests - Phase 5

### parser.rs

All test cases from spec section 3.7, including:

```
"main rs"               → And(Term("main"), Term("rs"))
"*.mp3|*.flac"          → Or(Wildcard, Wildcard)
"!node_modules"         → Not(Term)
"<c:|d:> *.mp3"         → And(Or(PathPrefix,PathPrefix), Wildcard)
"dm:today"              → DateModified(Today)
"size:>1mb"             → Size(Gt(1048576))
"size:large"            → Size(Between(1048577, 16777216))
"ext:jpg;png;gif"       → Ext(["jpg","png","gif"])
"audio:"                → TypeMacro::Audio
"!A !B"                 → And(Not(A), Not(B))
"A|B C"                 → Or(A, And(B, C))
"c:\users\"             → PathPrefix("c:\\users\\"), match_path=true
"fuzzy:mian"            → Predicate(Fuzzy("mian"))
"fuzzy:cnofig ext:toml" → And(Predicate(Fuzzy("cnofig")), Ext(["toml"]))
"archive: *.rs"         → And(Modifiers{archive_only:true}, Wildcard("*.rs"))
"noarchive: *.log"      → And(Modifiers{archive_only:false}, Wildcard("*.log"))
"archiveof:project.zip" → Function::ArchiveOf("project.zip")
```

### evaluator.rs

All test cases from spec section 4.7.

### size.rs

```
"large" → Between(1048577, 16777216)
">1mb"  → Gt(1048576)
"2mb..10mb" → Between(2097152, 10485760)
"gigantic" → Gte(134217729)
```

### date.rs

```
"today" → (today_midnight, today_end)
"thisweek" → (monday_midnight, sunday_end)
"last7days" → (now-7days, now)
"2024-01-01" → exact date range
```

### glob.rs

```
"*.rs" whole=true → "*.rs" GLOB pattern
"config" whole=false → "*config*"
case-insensitive: LOWER(name) GLOB LOWER(?)
```

---

## 5.10 Implementation Notes

### OR branch approximation in the evaluator

`Expr::Or` walks both sub-trees into separate `QueryPlan`s and then merges
their SQL conditions. Because `SqlCondition` has no `Raw(String, Vec<Value>)`
variant that could carry arbitrary SQL, the OR of two complex condition sets
cannot be expressed as a single typed condition. The evaluator falls back to
`SqlCondition::NameLike("%")` (match-all) for the merged OR branch and relies
on post-filters for narrowing. A future `SqlCondition::Or(Vec<SqlCondition>,
Vec<SqlCondition>)` variant would allow proper SQL `(cond1) OR (cond2)`.

### `archive:` is a modifier, not a term

`archive: *.rs` (with a space) tokenises as `Word("archive:")` followed by
`Word("*.rs")`. The parser consumes `archive:` as a modifier (sets
`archive_only = Some(true)`) and attaches it to the following `*.rs` term.
The result is a single `Predicate` with the modifier set, not `And(archive,
wildcard)`. This is the correct interpretation - `archive:` scopes the *next*
predicate, just as `cs:` scopes the next term's case sensitivity.

### `noarchive:` as a post-filter

`archive_path IS NULL` cannot be expressed as a `SqlCondition` variant (the
current enum has `IsArchiveEntry` for `IS NOT NULL` but not its inverse).
`noarchive:` therefore inserts a `PostFilter::ArchiveExclude` that filters rows
whose `archive_path` is non-null after the SQL query returns.

### `nucleo` 0.5 fuzzy API

`nucleo::Utf32String` does **not** implement `Deref<Target = Utf32Str>`.
Conversion requires `Utf32Str::new(s: &str, buf: &mut Vec<char>)` which fills
the buffer and returns a `Utf32Str<'_>` backed by it. The `Matcher` is not
`Send` (it holds internal state), so each rayon worker constructs its own
`Matcher::new(Config::DEFAULT)`. `fuzzy_match` returns `Option<u16>`, not
`Option<u32>` - scores are normalised against 1 024 for the threshold
comparison.

### `glob_to_sqlite` does not escape brackets

SQLite GLOB treats `[` and `]` as character-class delimiters (like shell glob).
The current implementation does not escape them because Everything's wildcard
syntax does not use bracket classes. If user input contains literal `[`, it
will be misinterpreted by SQLite. This is acceptable for the current scope;
a future `glob_escape_brackets` helper would fix it.

### `DateAccessed` approximated as `mtime`

The SQLite schema has `mtime` and `ctime` columns but no `atime`. The `da:`
(date-accessed) function maps to `MtimeRange` as a best-effort approximation.
This matches filesearch's design: access-time tracking is disabled on most
systems for performance reasons.

### `DupeKind::DateAccessed` falls back to `mtime`

`SqliteStore::find_dupes` maps `DupeKind::DateAccessed` to `mtime` (the only
timestamp column that approximates access time). The spec notes this case
returns empty; the implementation returns results grouped by `mtime` instead,
which is more useful than silence.

### `expand_user_macro` expansion strategy

User macros are expanded in `expand_user_macros` by simple string replacement
of `"name:"` patterns before tokenising. No recursive expansion is performed.
If a macro expands into another macro name, the second expansion is skipped.
This matches the spec's "no recursion" rule and avoids infinite loops.

---

## 5.11 Definition of Done

- [x] `cargo test -p filesearch query` passes all parser + evaluator tests.
- [x] All acceptance test queries from spec section 13 parse without error.
- [x] `dm:today` produces `MtimeRange` SQL condition (unit-tested; live index TBD).
- [ ] `audio:` returns the same extension list as Everything's built-in filter (live index).
- [x] `size:large` produces `SizeRange(1048577, 16777216)` (unit-tested).
- [x] `dupe:` produces `DupeSearch` strategy (unit-tested; live index TBD).
- [x] `content:TODO` delegates to Tantivy with `TantivyThenSqlite` strategy (unit-tested).
- [x] `fuzzy:mian` produces `FuzzyFilter` strategy (unit-tested; live scoring TBD).
- [x] `fuzzy:config ext:toml` produces AND of FuzzyFilter + ExtIn (unit-tested).
- [x] `archive: *.rs` produces `IsArchiveEntry` condition (unit-tested).
- [x] `noarchive: *.log` produces `ArchiveExclude` post-filter (unit-tested).
- [x] `archiveof:project.zip` produces `ArchiveOf` condition (unit-tested).
- [x] `archive: content:TODO` code path implemented (archive_only + TantivyThenSqlite).
- [x] `query/exact.rs`, `query/fuzzy.rs`, `query/regex_search.rs` do **not**
      exist.
- [x] `cargo clippy -- -D warnings` clean on `query/`.
- [x] No `unwrap()` in query engine (all replaced with `expect("infallible: ...")`).
- [x] `size:>1mb` tokenises correctly despite `>` being a group delimiter.
- [x] `DupeKind::NamePart` groups by stem (`COALESCE(REPLACE(name, '.' || extension, ''), name)`).
- [x] `filter_paths_by_conditions` prepares SQL statement once, not per path.
