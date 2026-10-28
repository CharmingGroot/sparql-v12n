# Query semantics

English | [한국어](semantics.ko.md) | [简体中文](semantics.zh-CN.md)

This document describes how sparql-v12n translates SPARQL queries into SQL and which rules decide the answers: column names, IRIs, literals, constants, joins, duplicates, `OPTIONAL`, `UNION`, `FILTER` and ordering. The [README](../README.md) gives an overview and the table of supported features.

## Column names

Column names in `rr:column`, in templates and in join conditions are SQL
identifiers (R2RML §5). They are resolved in one of two ways:

- A delimited name, such as `"\"Name\""`, is taken as written.
- A regular name is resolved as PostgreSQL resolves an unquoted name. This
  folds the name to lower case.

For example, `rr:column "StudentId"` reads the column that
`SELECT ... AS StudentId` produces, which is `studentid`. For the same
reason, `rr:column "Name"` does not match a column created as `"Name"`.

Before the engine runs a query that uses a regular name, it prepares a
probe statement in which PostgreSQL resolves the name. It then quotes the
resolved name in the generated SQL. Names made only of lower-case letters,
digits and `_` need no probe.

## Relative IRIs

A column or template with `rr:termType rr:IRI` can produce a relative IRI,
such as `Carlos` or `person/7`. R2RML resolves a relative IRI against a
base IRI given to the processor (R2RML §4, §11.2).

The resolution is plain string concatenation, not RFC 3986 resolution. For
example, with the base `http://data.example.com/`:

- `Carlos` becomes `<http://data.example.com/Carlos>`;
- `../x` becomes `<http://data.example.com/../x>`.

Template values are percent-encoded first. For example, `Bob/Charles`
gives `Bob%2FCharles`.

There is no default base IRI. The base IRI is set with `base_iri`,
`--base-iri` or `V12N_BASE_IRI`. Without it, a relative IRI is an R2RML
data error, and the error names the setting. In this way, the engine never
invents IRIs for the data. Set the base IRI to the namespace that the data
belongs to. It must be an absolute IRI.

A string counts as absolute when it starts with a scheme, such as `http:`
or `urn:`. A fragment does not make a string relative.

A string that has a scheme but is not a valid IRI is a data error. For
such a string, R2RML would first try the base IRI in front of it. The two
rules differ only for values such as `http://host:port/` with a
non-numeric port. The engine's rule lets the SQL comparisons below use the
same rule as the results.

- A template whose text never yields a scheme, such as `{name}` or
  `person/{id}`, gets the base IRI in front of it when the mapping is
  loaded. A constant such as `<http://data.example.com/person/7>` is then
  matched on the column as before (`id = $1`), using the column's index.
- A column can hold either form, relative or absolute. A constant therefore
  matches both forms, as in `iri IN ($1, $2)` with
  `http://data.example.com/a` and `a`. This condition still uses an index
  on the column.
- A join on such a column compares the generated IRIs in SQL. The same
  holds for a join on a template like `{scheme}:{rest}`, whose scheme
  depends on the values. The comparison has the form
  `CASE WHEN iri ~ '^scheme:' THEN iri ELSE $base || iri END`. An index on
  the column does not serve this comparison.
- Duplicates are removed by column value. A relative value and the absolute
  IRI it resolves to, such as `a` and `http://data.example.com/a`, therefore
  still count twice. For this reason, `SELECT DISTINCT` over such a column
  is rejected as unsupported when a base IRI is set.

For comparison, Ontop 5.5.0 puts `http://example.com/base/`, the base IRI
of the W3C test cases, in front of every template that does not start with
an absolute IRI. Ontop has no setting to change this base. It also fails
on relative IRIs from columns.

## Lexical forms of column values

The engine writes every non-string value in its canonical form. The
canonical form is the one that XML Schema 1.1 Part 2 defines, and this is
the version that R2RML references. The rule applies both to literals and
to template values:

- `numeric` 12.50 and 3.00 become `"12.5"^^xsd:decimal` and
  `"3"^^xsd:decimal`. An integral decimal has no decimal point.
- `float8` 30 becomes `"3.0E1"^^xsd:double`.
- `boolean` gives `true`/`false`.
- `bytea` gives upper-case hexadecimal.

Results therefore do not depend on a column's declared scale.

Ontop writes PostgreSQL's text output instead (`12.50`,
`"30"^^xsd:double`). The differential tests record these as known
differences (issue #25).

Two values that SQL `=` finds different can be one term. PostgreSQL keeps
`time '24:00:00'` apart from `'00:00:00'`. XSD 1.1 maps both to the same
value, written `00:00:00`. Both values give `"00:00:00"^^xsd:time`, with
these effects:

- The constant `"00:00:00"^^xsd:time` matches both.
- The constant `"24:00:00"^^xsd:time` matches nothing.
- Joins, `DISTINCT`, `UNION` and `ORDER BY` treat the two as one value
  (`col + interval '0'`).

A unique key on a `time` column is not used to prove that a query has no
duplicates. Ontop writes `24:00:00` as is (issue #37).

Two values that SQL `=` finds equal can still be different terms, as in
these cases:

- In `real` and `double precision` columns, `-0` and `0` give `"-0.0E0"`
  and `"0.0E0"`.
- A `char(n)` value keeps its padding, which `=` on `char(n)` ignores. For
  example, `'a'::char(3)` gives `"a  "`.

The engine compares such columns as terms in constants, joins, `sameTerm`,
and the duplicates that `DISTINCT` and `UNION` remove. In those places it
also compares the text of the value (for a zero) or its length (for
`char(n)`). It does this next to `=`, so that an index on the column still
finds the rows.

## Constants in a query

Constants in triple patterns are matched as RDF terms, as SPARQL requires.
The lexical form and the datatype must both be equal. A column value takes
part in the comparison through its R2RML natural mapping. For example,
`integer` columns give `xsd:integer` in canonical form, `numeric` gives
canonical `xsd:decimal`, and so on for other types.

The following table shows which constants match an integer column holding
7:

| Constant | Matches | Why |
|---|---|---|
| `7`, `<…/customer/7>` | yes | |
| `"7"`, `"7"^^xsd:decimal`, `7.0` | no | The datatype is different |
| `"07"^^xsd:integer`, `<…/customer/007>`, `<…/customer/+7>` | no | The constant is not the canonical form that the mapping produces |
| `99999999999`, `<…/customer/99999999999>` | no | The value is out of range for the column, so no row holds it |

Constants are sent to PostgreSQL as bound parameters, never inside the
SQL text.

Before binding, the engine checks each constant against the parameter
type that PostgreSQL infers from the column. The check has these steps:

1. The engine reads the constant as that type.
2. It formats the value back with the same code that builds result terms.
3. It sends the constant only if the round trip is identical. It sends
   the constant in binary where the type allows.

A constant that cannot match empties its own `UNION` branch. The engine
does this instead of making PostgreSQL fail on input it cannot parse.
Inside an `OPTIONAL` part, only that part does not match, and the
solutions on its left side are kept. When no branch is left, the query
skips the database.

The comparison stays on the column's own type, so the column's indexes are
used.

Some column types have no natural datatype, such as `interval`. For these
types, results carry the column cast to text. Constants are compared with
that same text (`col::text = $1`). This comparison cannot use an index on
the column.

## Literals

Literals are compared as RDF 1.1 terms everywhere. This covers triple
patterns, joins, `sameTerm`, `=` in `FILTER` where it means RDFterm-equal,
`DISTINCT`, and the results.

- Language tags compare without regard to case (RDF 1.1 Concepts §3.3).
  The engine therefore keeps them in lower case. For example,
  `rr:language "EN-us"` gives `"chat"@en-us`, and the constant
  `"chat"@EN-US` matches it. Two term maps that differ only in the case of
  their tags give one term. A tag that is not a valid BCP 47 language tag
  is a mapping error (R2RML §7.5). `rr:datatype rdf:langString` is also a
  mapping error, because `rdf:langString` is the datatype of tagged
  strings only.
- A simple literal is an `xsd:string` literal. The following all give the
  same term: `"x"`, `"x"^^xsd:string`, a text column, a column with
  `rr:datatype xsd:string`, and a literal template. Results write this
  term without a datatype. `DATATYPE` returns `xsd:string`.
- A datatype that no term map produces never matches in a triple pattern.
  For example, an integer column gives `xsd:integer` terms, so
  `"5"^^xsd:int` and `"5"^^xsd:decimal` match nothing there. A custom
  datatype matches nothing there either. In `FILTER`, numbers compare by
  value, and `xsd:float` and the derived integer types are rejected (see
  `FILTER`).
- `char(n)` values keep their trailing pad spaces in RDF terms. The SQL
  standard and the W3C RDB2RDF expected output have them this way (D018:
  `"Venus          "`). For unpadded values, use `varchar` or `text`, or
  an R2RML view with `rtrim()`.

Value comparisons across datatypes belong to `FILTER` (see below). An
example is `FILTER(?amount = 7.0)` on an integer column.

## Variables shared across columns

A variable that occurs twice is a join. A join also compares RDF terms,
not SQL values.

Before running such a query, the engine asks PostgreSQL for the types of
the columns involved. It does this by preparing a probe statement. The
engine then does the following:

- It compares columns of the same kind (integers, text, dates, ...) with
  `=`, so indexes are used.
- It compares other pairs through the natural lexical form computed in
  SQL (`int_col::text = text_col::text`). It first checks the natural
  datatypes. For example, an `integer` column holding 7 joins a text
  column only if that column has `rr:datatype xsd:integer`. It never joins
  a `numeric` 7.0. The canonical form of a `numeric` value is computed
  with `trim_scale(col)::text` (PostgreSQL 13 or later). A test checks
  that this form equals the engine's own formatting.
- When templates or IRI columns can only be compared as strings, it
  percent-encodes template values as IRIs do (R2RML IRI-safe version).
- When the lexical form cannot be computed in SQL, it rejects the query as
  unsupported (exit code 1, HTTP 501). An example is a `float8` or
  date/time template value against a text column. The engine rejects such
  a query instead of failing in PostgreSQL or returning wrong rows.

## Variables over columns of different types

The term maps of one pattern can read a variable from columns of
different SQL types. For example, the variable can come from an `integer`
column in one triples map and from a `text` column in another.

`UNION` needs one type per result column. For queries over several term
maps, the engine therefore also asks PostgreSQL for the types of the
selected columns. It then does the following:

- Where the types combine without changing any value, it keeps one result
  column. Examples are `int4` and `int8`, and `text` and `varchar`.
- It splits a literal of natural datatype into one result column per
  datatype. Each branch fills its own column and leaves the others `NULL`.
  As a result, the integer 120 stays `"120"^^xsd:integer` and the text
  `'120'` stays `"120"`.
- Where two types can give the same term, it selects the natural lexical
  form, computed in SQL as for joins. The same triple from two term maps
  then still counts once. This applies to the following terms:
  - template values (`item/{id}` over an `integer` key and a `text` key
    both give `item/1`);
  - IRI columns;
  - literals with `rr:datatype` or a language tag;
  - plain literals from `text` and `uuid` columns.
- When that form cannot be computed in SQL, it rejects the query as
  unsupported. This applies to `float8`, `real` with `double precision`,
  date/time, and `char(n)`.

## Duplicate triples

An RDF graph is a set of triples. A triple therefore counts once, however
many rows produce it. Several rows can produce the same triple in these
cases:

- duplicate rows of a table without a key;
- an R2RML view that repeats rows;
- two term maps that produce the same triple.

A basic graph pattern has one solution per distinct binding of all its
variables. Only after that does `SELECT` project. As a result,
`SELECT ?city WHERE { ?p ex:city ?city }` still returns a city once per
person.

The generated SQL removes duplicates with `DISTINCT`, or with `UNION`
across term maps. It does this over the columns of every variable of the
pattern. When some of those variables are not selected, it does this in a
subquery.

Some queries read tables (not views) through a single combination of term
maps (in some side of a `UNION`). Before running such a query, the engine
reads the unique keys of these tables from the catalog. When every table
has a key among the columns that the solution fixes, no two rows can give
the same solution. The SQL then keeps all rows without `DISTINCT`.

Otherwise, PostgreSQL has to deduplicate. This costs a hash or sort over
the rows. Unless an index delivers the rows in order, PostgreSQL also
reads all of them before it returns the first one.

`v12n explain` has no database and always shows the `DISTINCT`.

Duplicates are compared by column values. Two different values that a
template renders as the same string therefore still count twice. For
example, the template `{a}-{b}` renders the values `x-`, `y` and the
values `x`, `-y` as the same string.

A variable that is not selected is compared by the type and the text
output of its columns. Equal terms can therefore still count twice:

- columns of different types where SQL cannot compute the lexical form;
- columns whose types combine, such as `varchar` and `text`;
- values of one type whose text output differs, such as `numeric` 1.0 and
  1.00.

## OPTIONAL

`OPTIONAL` follows the LeftJoin of the SPARQL algebra. Every solution of
the left side is kept. Where the optional part has compatible solutions,
the left solution is extended by each of them. The SQL has one
`LEFT JOIN` per optional part.

- The tables of a part are joined among themselves. When there are
  several, they are in parentheses. The `ON` clause holds the conditions
  of the part itself and the link to the left side.
- A nested `OPTIONAL` is a `LEFT JOIN` inside the outer one. Patterns
  written after an `OPTIONAL` are joined after it.
- Shared variables are compared as RDF terms, as in joins. Typed columns
  and template decomposition therefore work the same way. Indexes are
  used where both sides are plain columns.
- An unbound variable is `NULL` in SQL and is left out of the JSON
  binding.
- A shared variable may be unbound on one side. This happens when it is
  bound only by an earlier `OPTIONAL`, or only by an `OPTIONAL` nested in
  the part. Such a variable is compatible with any value. The condition
  is then `(left IS NULL OR left = right)`, which no index on the column
  serves.
- Each part is translated on its own first, as the algebra evaluates it.
  A condition that links a nested part to the outer left side is
  therefore checked in the `ON` clause of the outer part. It is never
  checked inside the nested part.
- A variable that only `OPTIONAL` parts bind gets columns from each of
  those parts. Its value is the first one bound in the row.
- A constant term of an optional part comes with a flag column. The flag
  tells whether the part matched.
- Duplicate triples count once inside each part, as in any basic graph
  pattern.

An optional part can match in more than one way. This happens when more
than one combination of triples maps can match it, or when more than one
side of a `UNION` inside it can match. The part is then the union of
those choices, and the algebra gives the following identity.

LeftJoin(L, R1 ∪ R2) is Join(L, R1) ∪ Join(L, R2) ∪ Diff(L, R1 ∪ R2).

The SQL branch splits accordingly:

- One branch per choice. It has the `LEFT JOIN` of that choice and a
  `WHERE` condition that its part matched. PostgreSQL turns this into an
  inner join.
- One branch with the `LEFT JOIN` of every choice and a `WHERE` condition
  that none of them matched. This branch is an anti-join. It keeps the
  left solutions that nothing extends.

Each part keeps its `ON` clause. Compatibility and the `FILTER` inside
`OPTIONAL` therefore decide whether a part matches, as for a single part.
Inside an outer part, the splits become the choices of the outer part. A
`UNION` with an empty side extends every left solution unchanged through
that side.

The following are not supported yet and are rejected as unsupported:

- `SELECT DISTINCT` over a variable that only several `OPTIONAL` parts
  bind;
- a `FILTER` inside `OPTIONAL` around a `UNION` with an empty side.

## UNION

`{ A } UNION { B }` has the solutions of both sides as a bag. A solution
that both sides give counts twice. Under `SELECT DISTINCT` it counts
once. A variable that only one side binds is unbound in the solutions of
the other side.

- Joins, `FILTER`s and `OPTIONAL` distribute over a union on their left
  side. An **arm** is a choice of one side of every `UNION`. Because of
  this distribution, the engine unfolds each arm on its own and combines
  the arms with `UNION ALL`.
- Each arm is translated as any other pattern. Duplicate triples count
  once inside an arm. The arm uses `DISTINCT`, or `UNION` across its term
  maps, or nothing when keys prove that there are no duplicates. Keys are
  checked for each arm separately. The arms are never compared with each
  other.
- Each arm has one SQL branch per combination of term maps. The branches
  of all arms together are limited to 64, the same limit as for the
  branches of one pattern.
- A `FILTER` in one side applies only to the solutions of that side. A
  `FILTER` of the group around the union applies to the solutions of both
  sides. In those solutions, a variable of the other side is unbound.
- A `UNION` inside `OPTIONAL` does not distribute over the left join. Its
  sides are choices of the optional part (see `OPTIONAL`). The solutions
  of each side stay in their own arm.

A variable can take its values from term maps of different shapes. The
shapes can differ between the sides of a union or between term maps:

- a template in one, and a column or another template in the other;
- constants, IRIs and literals;
- literals of different datatypes or language tags.

The results rebuild each value from the term map of the branch that
produced the row. There are two cases.

- Terms of shapes that can never be the same get separate result
  columns. Each branch fills its own columns. Removing duplicates on
  these columns stays exact. Such shapes are:
  - two templates such as `http://example.org/customer/{id}` and
    `http://example.org/wholesale/{code}`, where the fixed text differs,
    or where one has text that IRI-safe encoding always escapes in the
    values of the other;
  - an IRI and a literal;
  - two different constants;
  - two language tags.
- Shapes that can give the same term share one result column when their
  duplicates are compared, in one arm or under `SELECT DISTINCT`. The
  column holds the string of the term. SQL computes it as for joins (the
  IRI-safe template, the IRI resolved against the base IRI, the natural
  lexical form). As a result, the same triple from a template and from an
  IRI column counts once. Such a column cannot use an index, and the
  string is computed for every row. This case is rejected as unsupported
  in two situations:
  - where SQL cannot compute the lexical form, as for joins;
  - for literals where one of the shapes is a column of natural datatype,
    whose datatype depends on the SQL type.

## FILTER

`FILTER` follows the evaluation rules of SPARQL, including errors. An
error is neither true nor false. `error || true` is true,
`error && false` is false, and `!error` is still an error.

The SQL uses `NULL` for an error. SQL's `AND`, `OR` and `NOT` treat
`NULL` the same way. `WHERE` keeps a row only where the filter is true.
A comparison that SPARQL makes an error is `NULL` in the SQL, never
`false`. As a result, `FILTER(!(?x = 5))` drops the rows where `?x` is
unbound or a string.

Where a filter applies:

- A `FILTER` in a group applies to the whole group, wherever it is
  written in the group. It sees only the variables of that group. For
  example, in
  `{ ?s ex:name ?n { ?s ex:qty ?q FILTER(?n = "x") } }` the filter is an
  error, because `?n` is not bound inside the nested group.
- A `FILTER` inside `OPTIONAL` goes into the `ON` clause of the
  `LEFT JOIN` of the part. It decides whether the part matches. It never
  decides which rows of the left side are kept. It sees the variables of
  the left side. Where it is an error, the part does not match, as with
  SQL's `LEFT JOIN`.
- A `FILTER` after the `OPTIONAL` is in `WHERE` and drops rows.
- An unbound variable is an error for every function except `BOUND`.
- A variable that only `OPTIONAL` parts bind is read from the part that
  matched (`CASE WHEN part matched THEN ... END`).

Comparisons follow the SPARQL operator mapping.

| Operands | `=`, `!=`, `<`, `<=`, `>`, `>=` |
|---|---|
| Numbers (`xsd:integer`, `xsd:decimal`, `xsd:double`) | By value, with XPath type promotion. `7.0` equals the integer 7, `-0` equals `0`, and NaN equals nothing. |
| Strings (simple literals, `xsd:string`) | By code point (`COLLATE "C"`, which needs a UTF-8 database). |
| `xsd:boolean` | `false` < `true`. |
| `xsd:dateTime` | By instant when both values have a time zone or neither has one. Comparing a value with a time zone and a value without one depends on an implicit time zone, and is rejected. |
| IRIs, blank nodes, and literals of other types | `=` and `!=` only, as RDF terms (RDFterm-equal). `<` and the other operators are an error. Two different literals are an error. |

Arithmetic (`+`, `-`, `*`, `/` and unary minus) is supported on
integers and decimals. The result follows XPath's type promotion: an
integer divided by an integer is a decimal (`7 / 2` is `3.5`). SQL
computes it in `numeric`, so a sum of large integers does not overflow
`int8`. A division by zero, a `numeric` NaN or infinity, and an operand
that is not a number are errors. Arithmetic on `xsd:double` values, and
`STR`, `LANG`, `DATATYPE` or `sameTerm` of an arithmetic result, are
rejected as unsupported for now (Discussion #144).

A constant compared with a column is converted to the type of the column
where that gives the same answer. An index on the column can then be
used. For example, `?qty > 5` is `qty > $1::int8`, and `?qty = 7.0` is
`qty = $1::int8`.

Otherwise the column is cast to the type that XPath promotes to, and no
index on the column serves the comparison. For example, `?qty < 5.5`
casts the integer column to `numeric`, and `?qty < 5.5e0` casts it to
`float8`.

`sameTerm`, `BOUND`, `isIRI`, `isBlank`, `isLiteral`, `STR`, `LANG` and
`DATATYPE` are supported. A term where a boolean is expected stands for
its effective boolean value. An empty string and zero are false.

`STR` and `DATATYPE` of a column work where its lexical form or natural
datatype is known in SQL, as for joins. The infinities of `date` and
`timestamp` are plain literals, so their `DATATYPE` is `xsd:string`.

`REGEX` translates the XPath regular expression into a PostgreSQL pattern
with the same matches. The pattern is applied with `~` under
`COLLATE "C"`.

| XPath | PostgreSQL pattern |
|---|---|
| `.` (without `s`) | Any character except newline and carriage return, `[^\u000A\u000D]`. |
| `^`, `$` | The start and end of the string, which are PostgreSQL's defaults. |
| `^`, `$` with `m` | Line boundaries, but not after a final newline (XPath 3.1), using lookaround. |
| `i` | Each character is expanded to its case variants (characters with equal `lower-case` or `upper-case`). `i` therefore also matches `ı`, and `k` also matches the Kelvin sign, whatever the database locale. |
| `\s` | Exactly space, tab, newline and carriage return. |
| `x` | Whitespace outside character classes is removed. |
| other characters | Written as `\uXXXX` escapes unless they are ASCII letters or digits. |

The following are rejected as unsupported:

- `\d`, `\w`, `\i`, `\c`, `\p{..}` and their complements, which are
  Unicode classes that PostgreSQL only has through the locale;
- back-references;
- character class subtraction;
- counts above 255;
- `\S` inside a class;
- the `q` flag;
- patterns or flags that are not constants.

An invalid pattern or flag is an error, as in XPath.

The following are rejected as unsupported, rather than answered
differently from engines that extend SPARQL:

- ordering or comparing two literals of the same datatype when that
  datatype is not in the table above, such as `xsd:date`;
- ordering or comparing two language-tagged literals, except with `=`
  outside a negation;
- comparisons over any of the following:
  - `real` columns;
  - values of a column whose `rr:datatype` is a numeric, boolean or
    dateTime datatype other than the natural datatype of the column;
  - `xsd:float` and the derived integer types;
  - dateTime constants outside the years 1 to 9999, or finer than a
    microsecond;
- other functions, arithmetic on `xsd:double`, `IN`, `EXISTS`, and
  expressions other than terms and arithmetic (variables, constants, and `STR`, `LANG` or `DATATYPE` of
  them) as operands;
- string ordering and `REGEX` in a database whose encoding is not UTF-8.

## BIND and SELECT expressions

`BIND(expr AS ?v)` and `SELECT (expr AS ?v)` give `?v` the value of the
expression in each solution (SPARQL 1.1 §18.5, Extend). The expression is
a variable, a constant, or arithmetic as in [`FILTER`](#filter):

- A variable gives its term, and a constant gives itself.
- Arithmetic gives a number whose datatype follows XPath's type
  promotion, written in canonical form (see [Literals](#literals)). Over
  an integer column, `?qty * 2` is an `xsd:integer` and `?qty / 4` is an
  `xsd:decimal`. The precision of a decimal division, which XPath leaves
  to the implementation, is that of PostgreSQL's `numeric` division.
- An error, such as a division by zero or an operand that is not a
  number, leaves `?v` unbound. The solution is kept.

SPARQL evaluates a `BIND` over the solutions of the group it is written
in. v12n computes it over the solutions of the whole pattern, after the
joins. Both give the same value when the rest of the pattern binds none
of the variables of the expression that the group may leave unbound.

The following are rejected as unsupported:

- expressions other than variables, constants and arithmetic, such as
  `STR`, comparisons, `IF` and `COALESCE`;
- `BIND` inside `OPTIONAL` or `UNION`;
- `BIND` over a variable that its group may leave unbound and a later
  part of the pattern binds;
- a variable that `BIND` or `SELECT` computes, used in a triple pattern,
  in `FILTER`, in `ORDER BY` or in another expression.

## ORDER BY, LIMIT and OFFSET

The solution modifiers apply in the order that the algebra defines
(SPARQL 1.1 §18.2.5). The order is `ORDER BY`, then projection,
`DISTINCT`, `OFFSET` and `LIMIT`. All of them are applied in the SQL.

`ORDER BY` takes variables and the expressions that `FILTER` supports.
Each of these comparators is `ASC` (the default) or `DESC`. Solutions
are sorted in SPARQL's order (§15.1), from first to last:

1. no value (an unbound variable, or an expression that is an error)
2. blank nodes
3. IRIs, by code point
4. literals. Where the `<` operator orders two literals, they are sorted
   by `<`, as in `FILTER`. Numbers sort by value with XPath type
   promotion, strings by code point, `false` before `true`, and
   dateTimes by instant.

Where SPARQL leaves the order open, the engine uses a fixed order of its
own. This order was decided in Discussion #63.

- Literals of different kinds sort in this order: numbers, booleans,
  dateTimes, strings, language-tagged literals, then other datatypes.
- Language-tagged literals sort by lexical form, then by language tag.
- Literals of other datatypes sort by datatype IRI. After that they sort
  by value for `date` and `time` columns, and by lexical form otherwise.
- NaN sorts after every number. `numeric` NaN and infinities sort where
  PostgreSQL puts them. Equal numbers of different datatypes (`1` and
  `1.0`) are in no particular order.
- The infinite values of `date` and `timestamp` columns are plain
  literals (`"infinity"`) and sort among the strings.

`DESC` reverses all of this order, so unbound values come last.

Each comparator becomes a few SQL sort keys (the kind of term, then a
value per kind). With these keys, values of different SQL types and of
different `UNION` branches sort together.

The SQL sorts by the column itself when one plain `SELECT` reads the
column of a comparator and nothing else can be in that column (a
required numeric, boolean or dateTime column). An index on the column
then delivers the rows in order. Strings and IRIs sort under
`COLLATE "C"`, which an index of another collation does not serve.

The sort keys are selected as extra columns and are never part of the
results. SQL keeps no order through a subquery, so the outermost query
decides the order in which the rows arrive. Every query that the engine
wraps around another (the row count, the cast of columns to text)
therefore sorts again by the sort keys.

`v12n explain` has no column types. It shows every column sorted as a
string.

`LIMIT` and `OFFSET` are bound parameters of the SQL. A query `LIMIT` at
or below the row limit leaves at most that many solutions, so the result
is never reported as truncated. When the query `LIMIT` is above the row
limit, the row limit still applies.

The following are rejected as unsupported:

- `ORDER BY` over a variable that `SELECT DISTINCT` does not project. A
  projected solution may stand for several solutions that the
  `ORDER BY` puts at different places.
- comparators over values with and without a time zone;
- comparators over `real` columns;
- comparators over strings in a database whose encoding is not UTF-8;
- as in `FILTER`, other functions, arithmetic and string forms that SQL
  cannot compute.
