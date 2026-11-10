# Reference: fs - Everything-Compatible Query Language

> Implements the full voidtools Everything search syntax plus `fuzzy:` and
> archive-scope extensions. This document specifies the `query/` module in
> `crates/fs-server/src/query/`.

---

## 1. Everything Syntax - Complete Reference

### 1.1 Boolean Operators

| Syntax | Meaning | Example |
|--------|---------|---------|
| `A B` (space) | AND | `main rs` |
| `A\|B` | OR | `*.jpg\|*.png` |
| `!A` | NOT | `!node_modules` |
| `<A B>` | Grouping | `<c:\|d:\> *.mp3` |
| `"A B"` | Literal phrase | `"program files"` |

Operator precedence (highest to lowest):
1. `<>` grouping
2. `!` NOT
3. space AND
4. `|` OR

Examples:
- `!A !B` = `(NOT A) AND (NOT B)`
- `A|B C` = `A OR (B AND C)`

`content:`, `fuzzy:` and `dupe:` choose how the whole query runs, so they
cannot appear inside `|` or after `!`; such queries are rejected with a
query error rather than evaluated with the wrong meaning.

### 1.2 Wildcards

| Wildcard | Meaning |
|----------|---------|
| `*` | Zero or more characters |
| `?` | Exactly one character |
| `*.` | Files with no extension |

Wildcards match the **whole filename** by default (controlled by `wfn:` modifier).
With `path:` they match the whole path, so `path: */src/*.rs` finds `.rs` files
anywhere under a `src` directory (`*` also matches `/`).

### 1.3 Modifiers

| Modifier | Aliases | Effect |
|----------|---------|--------|
| `case:` | | Case-sensitive matching |
| `nocase:` | | Case-insensitive (default) |
| `path:` | | Match against full path + filename |
| `nopath:` | | Match filename only (default) |
| `file:` | `files:` | Files only |
| `folder:` | `folders:` | Folders only |
| `regex:` | | Treat term as regex |
| `noregex:` | | Disable regex |
| `wildcards:` | | Enable wildcard matching |
| `nowildcards:` | | Disable wildcard matching |
| `wfn:` | `wholefilename:` | Match whole filename |
| `nowfn:` | `nowholefilename:`, `exact:` | Match anywhere in filename |
| `wholeword:` | `ww:` | Match whole words only |
| `nowholeword:` | `noww:` | Match anywhere (default) |
| `ascii:` | `utf8:` | Fast ASCII case comparison |
| `noascii:` | | Unicode-aware comparison |
| `diacritics:` | | Match accent marks exactly |
| `nodiacritics:` | | Ignore accent marks (default) |
| `archive:` | | Inner archive entries only (`archive_path IS NOT NULL`) |
| `noarchive:` | | Real filesystem files only (`archive_path IS NULL`) |

### 1.4 Functions

#### Date/Time Functions

| Function | Aliases | Description |
|----------|---------|-------------|
| `dm:<date>` | `datemodified:` | Date modified |
| `dc:<date>` | `datecreated:` | Date created |
| `da:<date>` | `dateaccessed:` | Date accessed |
| `rc:<date>` | `recentchange:` | Recently changed |
| `dr:<date>` | `daterun:` | Date run |

#### Size Functions

| Function | Description |
|----------|-------------|
| `size:<size>` | File size |

#### Name/Path Functions

| Function | Aliases | Description |
|----------|---------|-------------|
| `ext:<list>` | | Extension, semicolon-delimited: `ext:jpg;png;gif` |
| `parent:<path>` | `infolder:`, `nosubfolders:` | Files in exactly this folder |
| `child:<name>` | | Folders containing a child with this name |
| `startwith:<text>` | | Filename starts with text |
| `endwith:<text>` | | Filename ends with text |
| `len:<n>` | | Filename length |
| `depth:<n>` | `parents:` | Folder depth from root |
| `root:` | | No parent folder |

#### Attribute Functions

| Function | Description |
|----------|-------------|
| `attrib:<attrs>` | File attributes (see attribute constants below) |
| `type:<type>` | File type string |

#### Content Functions

| Function | Description |
|----------|-------------|
| `content:<text>` | Full-text search via Tantivy |
| `utf8content:<text>` | Treat file as UTF-8 |
| `ansicontent:<text>` | Treat file as ANSI |

#### Folder Stat Functions

| Function | Description |
|----------|-------------|
| `childcount:<n>` | Exactly N children |
| `childfilecount:<n>` | Exactly N files |
| `childfoldercount:<n>` | Exactly N subfolders |
| `empty:` | Empty folder |

#### Duplicate Functions

| Function | Description |
|----------|-------------|
| `dupe:` | Same filename as another file |
| `namepartdupe:` | Same name without extension |
| `sizedupe:` | Same size |
| `dmdupe:` | Same date modified |
| `dcdupe:` | Same date created |
| `dadupe:` | Same date accessed |
| `attribdupe:` | Same attributes |

#### Result Control

| Function | Description |
|----------|-------------|
| `count:<n>` | Limit results to N |
| `sort:<field>[-asc\|-desc]` | Order results by `name`, `size`, `dm` or `dc`. Without a direction, names run A-Z, sizes and dates largest and newest first |

#### Archive Functions (fs extension)

| Function | Description |
|----------|-------------|
| `archiveof:<path>` | All indexed inner files of a specific outer archive (glob supported) |

#### fs Extensions (not in voidtools Everything)

| Function | Description |
|----------|-------------|
| `fuzzy:<term>` | Fuzzy filename match using nucleo SIMD scorer |

### 1.5 Type Macros

| Macro | Expands to |
|-------|-----------|
| `audio:` | `ext:aac;ac3;aif;aifc;aiff;au;cda;dts;fla;flac;it;m1a;m2a;m3u;m4a;mid;midi;mka;mod;mp2;mp3;mpa;ogg;opus;ra;rmi;s3m;snd;umx;voc;wav;wma;xm` |
| `zip:` | `ext:7z;ace;arj;bz2;cab;gz;gzip;jar;r00;r01;r02;r03;r04;r05;r06;r07;r08;r09;rar;tar;taz;tgz;xz;z;zip` |
| `doc:` | `ext:c;chm;cpp;csv;cxx;doc;docm;docx;dot;dotm;dotx;h;hpp;htm;html;hxx;ini;java;lua;mht;mhtml;nfo;odp;ods;odt;pdf;potm;potx;ppam;pps;ppsm;ppsx;ppt;pptm;pptx;ps;py;rb;rc;rs;rtf;sldm;sldx;sql;svg;tex;toml;txt;vb;xlam;xls;xlsb;xlsm;xlsx;xltm;xltx;xml;yaml;yml` |
| `exe:` | `ext:bat;cmd;exe;msi;ps1;sh` |
| `pic:` | `ext:ani;bmp;gif;ico;jfif;jpeg;jpg;pcx;png;psd;tga;tif;tiff;webp` |
| `video:` | `ext:3g2;3gp;3gp2;3gpp;amr;amv;asf;avi;bdmv;bik;d2v;divx;drc;dsa;dsm;dss;dsv;evo;f4v;flc;fli;flic;flv;hdmov;ifo;ivf;m1v;m2p;m2t;m2ts;m2v;m4b;m4p;m4v;mkv;mov;mp2v;mp4;mp4v;mpe;mpeg;mpg;mpls;mpv2;mpv4;mqv;ogm;ogv;pss;pva;qt;ram;ratdvd;rm;rmm;rmvb;roq;rp;rpm;rsf;rts;smk;srt;swf;tp;tpr;ts;vob;vp6;webm;wm;wmp;wmv;wmx;wvx` |

### 1.6 Function Value Syntax

#### Comparison Operators

| Syntax | Meaning |
|--------|---------|
| `function:value` | Equal to value |
| `function:<value` | Less than |
| `function:<=value` | Less than or equal |
| `function:>value` | Greater than |
| `function:>=value` | Greater than or equal |
| `function:start..end` | Range (inclusive) |
| `function:start-end` | Range (inclusive) |

#### Size Values

| Suffix | Meaning |
|--------|---------|
| *(none)* | Bytes |
| `kb` | Kilobytes |
| `mb` | Megabytes |
| `gb` | Gigabytes |

Size constants:
- `empty` = 0 bytes
- `tiny` = 0 < size ≤ 10 KB → `Between(1, 10240)`
- `small` = 10 KB < size ≤ 100 KB → `Between(10241, 102400)`
- `medium` = 100 KB < size ≤ 1 MB → `Between(102401, 1048576)`
- `large` = 1 MB < size ≤ 16 MB → `Between(1048577, 16777216)`
- `huge` = 16 MB < size ≤ 128 MB → `Between(16777217, 134217728)`
- `gigantic` = size > 128 MB → `Gte(134217729)`

#### Date Values

Absolute: `YYYY`, `YYYY-MM`, `YYYY-MM-DD`, `YYYY-MM-DDThh:mm:ss`,
`YYYYMMDD`, locale-dependent formats (configurable).

Relative constants: `today`, `yesterday`, `thisweek`, `lastweek`,
`thismonth`, `lastmonth`, `thisyear`, `lastyear`, `last7days`, `last30days`,
month names (`january`–`december`, `jan`–`dec`),
day names (`monday`–`sunday`, `mon`–`sun`),
`last<N>hours`, `last<N>minutes`, `last<N>days`, `last<N>weeks`, `last<N>months`.

> **Important:** Date constants are resolved at **query-execution time**, not
> parse time. `dm:today` always means the current calendar day when the query runs.

#### Attribute Constants

| Letter | Attribute |
|--------|-----------|
| `A` | Archive | `C` | Compressed | `D` | Directory |
| `E` | Encrypted | `H` | Hidden | `I` | Not content indexed |
| `L` | Reparse point / symlink | `N` | Normal | `O` | Offline |
| `P` | Sparse | `R` | Read only | `S` | System | `T` | Temporary |

### 1.7 Character Escape Macros

| Macro | Character |
|-------|-----------|
| `quot:` | `"` | `apos:` | `'` | `amp:` | `&` |
| `lt:` | `<` | `gt:` | `>` |
| `#<n>:` | Unicode codepoint (decimal) |
| `#x<n>:` | Unicode codepoint (hex) |

---

## 2. AST (`query/ast.rs`)

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
    Term(String),           // plain text / wildcard
    Regex(String),          // regex: modifier
    Function(Function),     // dm:, size:, ext:, etc.
    TypeMacro(TypeMacro),   // audio:, zip:, doc:, etc.
    Fuzzy(String),          // fuzzy:<term> - fs extension
}

pub struct Modifiers {
    pub case_sensitive:    Option<bool>,   // None = false (default)
    pub match_path:        Option<bool>,   // None = false (default)
    pub files_only:        Option<bool>,
    pub folders_only:      Option<bool>,
    pub whole_filename:    Option<bool>,
    pub whole_word:        Option<bool>,
    pub ignore_diacritics: Option<bool>,   // None = true (default)
    /// None  = include both real files and inner archive entries.
    /// Some(true)  = archive: modifier - inner entries only.
    /// Some(false) = noarchive: modifier - real filesystem files only.
    pub archive_only:      Option<bool>,
}

pub enum Function {
    // Date
    DateModified(DateExpr), DateCreated(DateExpr), DateAccessed(DateExpr),
    DateRun(DateExpr), RecentChange(DateExpr),
    // Size
    Size(RangeExpr<u64>),
    // Name/path
    Ext(Vec<String>), Parent(String), Child(String),
    StartWith(String), EndWith(String),
    Len(RangeExpr<u64>), Depth(RangeExpr<u64>), Root,
    // Attributes
    Attrib(Vec<AttributeFlag>), Type(String),
    // Content
    Content(String), Utf8Content(String), AnsiContent(String),
    // Folder stats
    ChildCount(RangeExpr<u64>), ChildFileCount(RangeExpr<u64>),
    ChildFolderCount(RangeExpr<u64>), Empty,
    // Duplicates
    Dupe, NamePartDupe, SizeDupe, DateModifiedDupe,
    DateCreatedDupe, DateAccessedDupe, AttribDupe,
    // Result control
    Count(u64),
    // Archive (fs extension)
    ArchiveOf(String),  // find all inner files of a specific outer archive
}

pub enum RangeExpr<T> { Eq(T), Lt(T), Lte(T), Gt(T), Gte(T), Between(T, T) }

pub enum DateExpr {
    Absolute(RangeExpr<chrono::NaiveDateTime>),
    Constant(DateConstant),
    Range(DateConstant, DateConstant),
}

pub enum DateConstant {
    Today, Yesterday, ThisWeek, LastWeek, ThisMonth, LastMonth, ThisYear, LastYear,
    LastNDays(u32), LastNHours(u32), LastNMinutes(u32), LastNWeeks(u32), LastNMonths(u32),
    Month(u32),    // 1–12
    Weekday(u32),  // 0=Sunday, 6=Saturday
}

pub enum TypeMacro { Audio, Zip, Doc, Exe, Pic, Video }

pub enum AttributeFlag {
    Archive, Compressed, Directory, Encrypted, Hidden,
    NotContentIndexed, ReparsePoint, Normal, Offline,
    Sparse, ReadOnly, System, Temporary,
}
```

---

## 3. Parser (`query/parser.rs`) [tested]

### 3.1 Input Normalisation (before tokenising)

1. Strip leading/trailing whitespace.
2. Expand character escape macros (`quot:` → `"`, etc.).
3. Expand type macros (`audio:` → `ext:aac;...`).
4. Resolve user-defined macros from config (no recursion).

### 3.2 Token Types

```rust
pub enum Token {
    Word(String),
    QuotedPhrase(String),
    Pipe,
    Bang,
    LAngle,
    RAngle,
    ModifierPrefix(String),
    Colon,
    FunctionValue(String),
    Eof,
}
```

### 3.3 Grammar (Recursive Descent)

```
expr       := or_expr
or_expr    := and_expr ("|" and_expr)*
and_expr   := not_expr (not_expr)*    ← space = AND
not_expr   := "!" atom | atom
atom       := "<" expr ">" | quoted_phrase | term
term       := modifier* (function | word_pattern)
```

### 3.4 Modifier Resolution

When `word:` is encountered:
1. Known **modifier** keyword → set flag on next term's `Modifiers`.
2. Known **function** keyword → parse as `Function` with value.
3. Known **type macro** → `TypeMacro` variant.
4. Followed by `\` → treat as Windows path prefix with `match_path: true`.
5. Otherwise → literal word (colon is part of the text).

### 3.5 Error Handling

```rust
#[derive(thiserror::Error, Debug)]
pub enum QueryError {
    #[error("Invalid regex: {0}")]
    InvalidRegex(String),
    #[error("Unknown function: {0}")]
    UnknownFunction(String),
    #[error("Invalid value for {function}: {value} - {reason}")]
    InvalidValue { function: String, value: String, reason: String },
    #[error("Unmatched grouping character")]
    UnmatchedGrouping,
    #[error("Unexpected end of input")]
    UnexpectedEof,
}
```

`InvalidRegex` → 400 Bad Request.
`UnknownFunction` → treat as plain text term.

### 3.6 Parser Tests

```
"main rs"               → And(Term("main"), Term("rs"))
"*.mp3|*.flac"          → Or(Wildcard("*.mp3"), Wildcard("*.flac"))
"!node_modules"         → Not(Term("node_modules"))
"<c:|d:> *.mp3"         → And(Or(PathPrefix, PathPrefix), Wildcard("*.mp3"))
"dm:today"              → Function::DateModified(DateConstant::Today)
"size:>1mb"             → Function::Size(Gt(1048576))
"size:large"            → Function::Size(Between(1048577, 16777216))
"ext:jpg;png;gif"       → Function::Ext(["jpg","png","gif"])
"audio:"                → TypeMacro::Audio
"!A !B"                 → And(Not(Term("A")), Not(Term("B")))
"A|B C"                 → Or(Term("A"), And(Term("B"), Term("C")))
"c:\users\"             → PathPrefix("c:\\users\\"), match_path=true
"fuzzy:mian"            → Predicate(Fuzzy("mian"))
"fuzzy:cnofig ext:toml" → And(Predicate(Fuzzy("cnofig")), Ext(["toml"]))
"archive: *.rs"         → And(Modifiers{archive_only:true}, Wildcard("*.rs"))
"noarchive: *.log"      → And(Modifiers{archive_only:false}, Wildcard("*.log"))
"archiveof:project.zip" → Function::ArchiveOf("project.zip")
"regex:[^\x00-\x7f]"   → Regex("[^\\x00-\\x7f]")
```

---

## 4. Sub-parsers

### 4.1 size.rs [tested]

```rust
pub fn parse_size_value(s: &str) -> Result<RangeExpr<u64>, QueryError>
```

Handles: comparison prefix (`<`, `<=`, `>`, `>=`, `=`), ranges (`1mb..10mb`,
`1mb-10mb`), unit suffixes (`kb`, `mb`, `gb`, case-insensitive), named constants.

Tests:
```
"large"     → Between(1048577, 16777216)
">1mb"      → Gt(1048576)
"2mb..10mb" → Between(2097152, 10485760)
"gigantic"  → Gte(134217729)
```

### 4.2 date.rs [tested]

```rust
pub fn parse_date_value(s: &str) -> Result<DateExpr, QueryError>
pub fn resolve_date_constant(c: &DateConstant, now: DateTime<FixedOffset>) -> (i64, i64)
```

`now` carries the caller's UTC offset (e.g. `Local::now().fixed_offset()`) so
day/week/month boundaries resolve against the caller's local calendar day
while still returning real UTC epoch seconds comparable against stored
mtimes.

`resolve_date_constant` examples:
- `Today` → midnight..23:59:59 of current local day
- `Yesterday` → previous day
- `ThisWeek` → Monday 00:00..Sunday 23:59 of current week
- `LastNDays(n)` → now − n×86400..now
- Month names → first..last day of that month (current year)
- Weekday names → most recent occurrence (00:00..23:59)

Tests:
```
"today"      → (today_midnight, today_end)
"thisweek"   → (monday_midnight, sunday_end)
"last7days"  → (now-7days, now)
"2024-01-01" → exact date range
```

### 4.3 glob.rs [tested]

```rust
pub fn glob_to_sqlite(pattern: &str, whole_filename: bool) -> String
```

- `*` → `*`, `?` → `?` (SQLite GLOB syntax)
- `whole_filename = false` → wrap: `"*" + pattern + "*"`
- Case-insensitive: `LOWER(name) GLOB LOWER(?)`

### 4.4 macros.rs

```rust
pub fn expand_type_macro(m: TypeMacro) -> Vec<String>    // returns ext list
pub fn expand_user_macro(name: &str, macros: &MacroConfig) -> Option<String>
```

User macros are expanded before tokenising. No recursive expansion.

---

## 5. Evaluator (`query/evaluator.rs`) [tested]

### 5.1 QueryPlan

```rust
pub struct QueryPlan {
    pub sql_conditions: Vec<SqlCondition>,
    pub tantivy_query:  Option<String>,
    pub post_filters:   Vec<PostFilter>,
    pub limit:          Option<usize>,
    pub offset:         usize,
    pub strategy:       EvalStrategy,
}

pub enum EvalStrategy {
    SqliteOnly,
    TantivyThenSqlite,
    DupeSearch(DupeKind),
    /// Fetch SQL candidates, then score with nucleo in rayon par_iter.
    FuzzyFilter(String),
}

pub enum SqlCondition {
    NameLike(String),
    NameGlob(String),
    NameGlobNoCase(String),
    PathLike(String),
    ExtIn(Vec<String>),
    SizeRange(u64, u64),
    SizeOp(CmpOp, u64),
    MtimeRange(i64, i64),
    MtimeOp(CmpOp, i64),
    CtimeRange(i64, i64),
    CtimeOp(CmpOp, i64),
    IsDir(bool),
    IsHidden(bool),
    IsSymlink(bool),
    ParentPath(String),
    PathPrefix(String),
    NameLen(CmpOp, u64),
    Depth(CmpOp, u64),
    IsArchiveEntry(bool),  // true = archive_path IS NOT NULL; false = IS NULL
    ArchiveOf(String),     // archive_path = ? or GLOB ?
    Not(Box<SqlCondition>),
    And(Vec<SqlCondition>),
    Or(Vec<SqlCondition>),
}

pub enum PostFilter {
    Regex(regex::Regex, bool),
    Child(String),
    ChildCount(CmpOp, u64),
    Attrib(Vec<AttributeFlag>),
    WholeWord(String),
    StartWith(String),
    EndWith(String),
}
```

### 5.2 AST → QueryPlan Dispatch

```rust
pub fn plan(expr: &Expr, default_modifiers: &Modifiers) -> Result<QueryPlan, QueryError>;
```

| Predicate | Maps to |
|-----------|--------|
| Plain term (no wildcards) | `NameLike("%term%")` |
| Term with `*`/`?` | `NameGlobNoCase(glob_to_sqlite(term))`; `NameGlob` with `case:` |
| `path:` modifier | `PathLike` instead of `NameLike`; with wildcards, `PathLike(glob_to_like(term))` over the whole path |
| `regex:` modifier | compile regex → `PostFilter::Regex` |
| `file:` modifier | `IsDir(false)` |
| `folder:` modifier | `IsDir(true)` |
| `dm:`, `dc:`, `da:` | resolve `DateExpr` → `MtimeRange`/`CtimeRange` |
| `size:` | `SizeRange` or `SizeOp` |
| `ext:list` | `ExtIn(list)` |
| `parent:path` | `ParentPath(path)` |
| `content:text` | `tantivy_query = Some(text)`, strategy = `TantivyThenSqlite` |
| `fuzzy:term` | strategy = `FuzzyFilter(term)` |
| `dupe:` | strategy = `DupeSearch(DupeKind::Name)` |
| `count:n` | `plan.limit = n` |
| `sort:field` | `plan.sort = Sort { key, descending }` |
| `len:` | `NameLen(op, val)` |
| `depth:` | `Depth(op, val)` |
| `attrib:` | `PostFilter::Attrib(flags)` |
| `child:` | `PostFilter::Child(name)` |
| `startwith:` | `PostFilter::StartWith(text)` |
| `endwith:` | `PostFilter::EndWith(text)` |
| TypeMacro | expand → `ExtIn` |
| `archive:` modifier | `IsArchiveEntry(true)` |
| `noarchive:` modifier | `IsArchiveEntry(false)` |
| `archiveof:path` | `ArchiveOf(path)` |

SQL generation for archive conditions:
```sql
-- IsArchiveEntry(true):   archive_path IS NOT NULL
-- IsArchiveEntry(false):  archive_path IS NULL
-- ArchiveOf("/data/docs.zip"):  archive_path = '/data/docs.zip'
-- ArchiveOf("*.zip"):           archive_path GLOB '*.zip'
```

### 5.3 `fuzzy:` Behaviour

`fuzzy:<term>` is a first-class query function that does **not** exist in
voidtools Everything.

1. Fetch candidate `FileRecord`s from SQLite matching all other `SqlCondition`s
   (no name condition is added to SQL for the fuzzy term itself).
2. Score candidates against `<term>` using `nucleo::Matcher` in `rayon::par_iter`.
3. Retain results above `config.search.fuzzy_threshold` (default 0.7).
4. Order by score descending.
5. Apply `limit` / `offset`.

Compatible with all other query functions:
- `fuzzy:config ext:toml` - only `.toml` files, fuzzy-matched against "config"
- `fuzzy:readm size:<100kb` - small files, fuzzy-matched against "readm"

### 5.4 Execution Strategies

| Strategy | When | How |
|----------|------|-----|
| `SqliteOnly` | No `content:`, no `fuzzy:` | Build SQL, query SQLite |
| `TantivyThenSqlite` | `content:` present | Tantivy → path list → SQLite metadata filter |
| `DupeSearch(kind)` | `dupe:` present | Two-pass GROUP BY → IN |
| `FuzzyFilter(term)` | `fuzzy:` present | SQL candidates → nucleo par_iter |

### 5.5 Dupe Search (`query/dupes.rs`) [tested]

```rust
pub fn find_dupes(
    sqlite:           &SqliteStore,
    kind:             DupeKind,
    extra_conditions: &[SqlCondition],
    limit:            usize,
) -> Result<Vec<FileRecord>>
```

Two-pass query:
```sql
-- Pass 1: find duplicated values
SELECT <column>, COUNT(*) as c FROM files
WHERE <extra_conditions>
GROUP BY <column> HAVING c > 1;

-- Pass 2: fetch all files with those values
SELECT * FROM files
WHERE <column> IN (values from pass 1)
  AND <extra_conditions>
LIMIT ?;
```

`DupeKind` variants: `Name` → `name`; `NamePart` → `REPLACE(name, extension, '')`;
`Size` → `size_bytes`; `DateModified` → `mtime`; `DateCreated` → `ctime`;
`Attrib` → `attrib_bitmask`.

### 5.6 Post-filter Application

```rust
pub fn apply_post_filters(
    records: Vec<FileRecord>,
    filters: &[PostFilter],
    sqlite:  &SqliteStore,
) -> Vec<FileRecord>
```

### 5.7 Evaluator Tests

```
"dm:today"              → mtime covers today's midnight..23:59
"size:large"            → SizeRange(1048577, 16777216)
"*.mp3 dm:thisweek"     → And(NameGlob("*.mp3"), MtimeRange(week_start, week_end))
"ext:jpg;png"           → ExtIn(["jpg","png"])
"audio:"                → ExtIn([...audio extensions...])
"dupe: *.mp4"           → DupeSearch(Name) + NameGlob("*.mp4")
"file: *.rs"            → IsDir(false) + NameGlob("*.rs")
"content:TODO dm:week"  → TantivyThenSqlite + MtimeRange(...)
"len:>20"               → NameLen(Gt, 20)
"depth:<=2"             → Depth(Lte, 2)
"count:25 *.log"        → limit=25, NameGlob("*.log")
"archive: *.rs"         → IsArchiveEntry(true) + NameGlob("*.rs")
"noarchive: *.log"      → IsArchiveEntry(false) + NameGlob("*.log")
"archiveof:project.zip" → ArchiveOf("project.zip")
```

---

## 6. Public Entry Point (`query/mod.rs`)

```rust
pub async fn execute(
    raw_query: &str,
    sqlite:    Arc<SqliteStore>,
    tantivy:   Arc<TantivyStore>,
    config:    Arc<RwLock<Config>>,
) -> Result<QueryResult, QueryError>
```

Internal flow:
1. Expand user macros (`config.macros`).
2. Expand character escape macros.
3. Expand type macros.
4. Tokenise.
5. Parse → `Expr` AST.
6. Plan → `QueryPlan`.
7. Execute (strategy dispatch).
8. Apply post-filters.
9. Apply limit/offset.
10. Return `QueryResult`.

---

## 7. User-Defined Macros (`config.toml`)

```toml
[macros]
work = "path:/home/user/projects"
pics = "pic: dm:thismonth"
big  = "size:gigantic"
```

Macros are expanded before tokenising. No recursion.

---

## 8. Acceptance Tests

All of the following must parse without error and return correct results:

```
# Basic
main.rs
*.log
"program files"
!*.tmp

# Boolean
*.jpg|*.png
src !test
<c:|d:> *.mp3

# Modifiers
case:README.md
path:src *.rs
file: *.toml
folder: src

# Date
dm:today
dm:thisweek
dm:last7days
dm:2024-01-01..2024-12-31
dc:yesterday

# Size
size:>1mb
size:large
size:100kb..1mb
size:gigantic

# Extension
ext:rs;toml;md
audio:
video: size:>500mb

# Path
parent:src
d:\projects\
documents\

# Wildcards
*.?
*config*
test_?.rs

# Duplicates
dupe:
sizedupe: size:>100mb

# Content
content:TODO
content:fixme ext:rs dm:thisweek

# Fuzzy (fs extension)
fuzzy:mian
fuzzy:config ext:toml
fuzzy:readm size:<100kb

# Archive scope (fs extension)
archive: *.rs
noarchive: *.log
archive: content:TODO
archiveof:/data/project.zip
archiveof:*.zip *.rs
archive: dm:today
archive: size:>1mb

# Combined
*.log size:>10mb dm:today !archive
path:src ext:rs dm:thisweek content:unwrap
video: size:>1gb !<backup|temp>
```

---

## 9. Compatibility Notes

### Intentional deviations from Everything

| Feature | Everything | fs | Reason |
|---------|-----------|------------|--------|
| `content:` | Unindexed, slow | Indexed via Tantivy | Core feature |
| `~` prefix | Fuzzy match | Not supported - use `fuzzy:` | Replaced by first-class function |
| `/` prefix | Regex | Not supported - use `regex:` | Replaced by modifier |
| ID3/FLAC tag functions | Supported | Not in v1 | Out of scope |
| Image dimension functions | Supported | Not in v1 | Out of scope |
| `frn:` / `fsi:` | NTFS-specific | Not supported | Not cross-platform |
| `shell:` | Windows shell folders | Not supported | Windows-only |
| `runcount:` / `dr:` | App launch tracking | Not supported | No launch tracking |

### Additions beyond Everything

| Feature | Description |
|---------|-------------|
| `fuzzy:` | Nucleo SIMD fuzzy match - first-class query function |
| `archive:` / `noarchive:` | Scope queries to inner ZIP entries or real files |
| `archiveof:` | Find all inner files of a specific ZIP |
| `content:` indexing | Tantivy-backed, < 100ms |
| gRPC / REST / IPC APIs | Everything only has HTTP + SDK |
