We have a bunch of csvs of unknown format.

They come every month. So the format could be like file1.csv, file2.csv. Files change in certain fields.

The CSVs contain account information every month. Every row is an account. The columns shouldn't change.

Use Rust.
We need to first identify what type of data is in every CSV. We then need to identify unique values. For example, a loan could be "fixed" or "variable" and those could be the only fields.
Some monthly CSVs may not have every field type, e.g., enum_a might not have every unique value occur in Month A. Some rare value may pop up only in month B.

Most data is repeated per month. We don't actually want to store the exact data again. Only the deltas.
We want to use a Postgres 18 temporal style, with the WITHOUT OVERLAPS new syntax to track EVERY attribute as a sseparat table. For exampke, customer name might change so customer_name is a separate table. Look in ~/fj/ for some other schemas that I have created before (you can search by *.sql extension and see that sometimes. I make schemas using this Postgres style. Maybe my contact manager in acme-internal-site.

So basically I will feed in a list from a plain text file like

month1/a.csv
month2/a.csv
month3/a.csv

And you should be able to process them in order ,store the detlas using upserts (using daterange usually column called during)


Write everything in Rust using plain postgres driver. Don't use thiserror or anyhow, hand roll errors. Refer to my Rust style in ohter craets in ~/fj. Use Rust.

---

## Implementation

`csv-pg-schema` is a Rust binary (`tokio-postgres`, hand-rolled errors, no
`thiserror`/`anyhow`). It works in two steps, both driven by a plain-text
list of CSV paths.

### The list file

One CSV per line, processed top to bottom. An optional second field is the
observation instant for that month (`YYYY-MM-DD` or a full timestamp). If
it's omitted, the date is lifted from the path (e.g. `…/2024-03/…`), and if
no line has any date at all, synthetic consecutive months are used so a bare
`month1/a.csv` list still loads. Blank lines and `#` comments are ignored.

```
month1/accounts.csv  2024-01-01
month2/accounts.csv  2024-02-01
month3/accounts.csv  2024-03-01
```

### 1. analyze — infer the schema

Scans every listed CSV once, infers each column's type (`boolean`,
`integer`, `bigint`, `numeric`, `date`, `timestamptz`, else `text` —
integer columns stay `integer`/`i32` until a value needs the wider
`bigint`/`i64`), and flags low-cardinality columns as enum-like — listing
the distinct values it saw.
Because it scans *all* months together, a rare value that only shows up in a
later month is still captured. It also **compares the files** to see which
columns actually change over time, and emits a history table **only** for
those — a column that never changes is just a plain column on `account`,
since it needs no history. It prints the temporal `CREATE` script:

```sh
csv-pg-schema analyze --list example/list.txt          # to stdout
csv-pg-schema analyze --list example/list.txt -o schema.sql
```

The generated schema follows the one-table-per-*changing*-attribute temporal
style:

- `account` — the entity, keyed by the CSV's natural key (auto-detected, or
  `--key COL`), mapped to a surrogate `id`. **Static attributes** (those that
  never change across the inputs) are plain columns right here.
- `account_<attr>` — one history table per **changing** attribute, each row
  `(id, <attr>, during tstzrange, request_id)` with
  `PRIMARY KEY (id, during)` and `UNIQUE (id, during WITHOUT OVERLAPS)`.
- `import_request` — one row per processed file (provenance + the
  observation instant shared by every range it writes).

(With only one file there is nothing to compare, so every attribute is kept
temporal.)

Pass `--with-checks` to bake the inferred value profile into the DDL instead
of just commenting it: numeric columns take a `numeric(p, s)` type and a
range check, low-cardinality text columns get an `IN (...)` membership check,
and other text columns get length and character-class checks:

```sql
balance      numeric(7,2) NOT NULL CHECK (balance BETWEEN -420.00 AND 30000.00),
currency     text NOT NULL CHECK (currency IN ('CAD', 'EUR', 'USD')),
holder_name  text NOT NULL CHECK (char_length(holder_name) BETWEEN 8 AND 17),
```

A non-enum text column that shares a character class instead gets a regex
check (e.g. an all-uppercase code becomes `CHECK (code ~ '^[A-Z]+$')`). These
describe only what the input files actually contained, so they are opt-in: a
later month with an out-of-range value would violate them. `load
--with-checks --create-schema` applies the same constrained DDL.

### 2. report — compare the CSVs month over month

Diffs the listed CSVs in memory (no database) and prints a comparison
report, so you can see what's actually changing before loading anything:

```sh
csv-pg-schema report --list example/list.txt
csv-pg-schema report --list example/list.txt -o report.txt
```

It shows:

- **per-file overview** — rows, accounts added/removed, and how many
  accounts/cells changed versus the previous month;
- **columns** — an aligned table with one row per column: inferred type,
  nullability, a **changes** flag (`yes`/`no` — does it differ month to
  month, the same signal that decides temporal vs. plain column; `?` with a
  single file), and a **summary** of the inferred constraints: the
  **distinct-value count** turned into a schema hint (`< 5` → enum candidate,
  `5–10` → borderline, `> 10` → its own lookup table), the observed **range**
  (numeric/date), a tight **`numeric(p, s)`** suggestion plus the **min/max
  decimal places**, **text length** range, **character class** (e.g.
  *uppercase only*, *digits only*), and the **null count** when present;
- **attribute changes** — for each attribute, how many accounts changed it
  in each month-to-month transition (attributes that never change are
  omitted);
- **value transitions** — the actual `account: old → new` changes per
  attribute per transition (capped per attribute/month);
- **distinct values & first appearance** — every distinct value with the
  month it *first* appeared, so a value that only shows up later (e.g. a new
  `status`) stands out;
- **account lifecycle** — distinct accounts, how many persist across every
  month, and which were added later or disappeared.

By default the run is **strict**: a ragged row, a blank account key, or a
file whose columns differ from the first file's all abort with the offending
file named. Pass `--lenient` to tolerate these (skip bad rows, union
headers) as earlier versions did.

The columns table looks like this (from `testdata/`):

```
columns (key + 8 attributes; "changes" = differs month to month):
  column        type     nullable  changes  summary
  account_id    integer  key       -        distinct 6  ·  range 2001 .. 2006
  holder_name   text     not null  yes      distinct 7 -> borderline (5-10)  ·  length 8-17  ·  ASCII only
  currency      text     not null  yes      distinct 3 -> enum candidate (<5)  ·  length 3  ·  uppercase letters only
  balance       numeric  not null  yes      distinct 21 -> lookup table candidate (>10)  ·  range -420.00 .. 30000.00  ·  fits numeric(7,2)  ·  2 decimal(s)
  credit_limit  numeric  nullable  yes      distinct 3 -> enum candidate (<5)  ·  range 2000.00 .. 15000.00  ·  fits numeric(7,2)  ·  2 decimal(s)  ·  15/21 null
  region_code   text     not null  no       distinct 2 -> enum candidate (<5)  ·  length 2  ·  uppercase letters only
  opened_on     date     not null  no       distinct 6 -> borderline (5-10)  ·  range 2018-01-09 .. 2023-09-12
```

So `region_code` and `opened_on` never change and become plain columns on
`account`; everything marked `yes` gets its own temporal history table.

### 3. rust — generate an `InputRecord` struct

Emits Rust source for the inferred schema: an `InputRecord` struct with one
field per column, plus a Rust enum for every genuinely categorical column.

```sh
csv-pg-schema rust --list example/list.txt           # to stdout
csv-pg-schema rust --list example/list.txt -o input_record.rs
```

The field types mirror the inferred Postgres types: `integer`/`bigint` →
`i32`/`i64`, `numeric` → `f64`, `boolean` → `bool`, and `date`/`timestamptz`/
`text` → `String` (no date crate is pulled in). A nullable column becomes
`Option<T>`. A `text` column whose cardinality is in the `< 5` enum-candidate
band becomes its own enum (with `as_str` and `FromStr`), and that enum is the
field's type — so the "a loan is fixed or variable" columns are typed, while
names, dates, and numerics keep their scalar types.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Active,
    Closed,
    Dormant,
    Frozen,
}
// ... as_str / FromStr impls ...

#[derive(Debug, Clone, PartialEq)]
pub struct InputRecord {
    pub account_id: i32,
    pub holder_name: String,
    pub currency: Currency,
    pub balance: f64,
    pub credit_limit: Option<f64>,
    pub status: Status,
    pub opened_on: String,
}
```

### 4. load — store only the deltas

Re-infers the same schema (including the same temporal-vs-static decision),
then processes the files in order. Static attributes are written once, inline
on the `account` row when the account is first seen. Each **changing**
attribute write is an *upsert in time*: the current open range is closed at
the observation instant **only if the value changed**, and a new
`[at, infinity)` range opens **only if** none already covers that instant.
An unchanged value writes nothing, so repeated monthly data is not
re-stored. Re-running the same load is idempotent for the attribute tables.

```sh
export DATABASE_URL='postgres://user@localhost/accounts'
csv-pg-schema load --list example/list.txt --create-schema   # apply DDL + load
csv-pg-schema load --list example/list.txt                   # schema already applied
```

`--create-schema` applies the generated DDL first (needs `btree_gist`,
which the script creates, and Postgres 18 for `WITHOUT OVERLAPS`).

A point-in-time snapshot is then a join on `during @> $ts`:

```sql
SELECT a.account_number, n.customer_name, b.balance, s.status
FROM account.account a
JOIN account.account_customer_name n ON n.id = a.id AND n.during @> '2024-02-15'::timestamptz
JOIN account.account_balance       b ON b.id = a.id AND b.during @> '2024-02-15'::timestamptz
JOIN account.account_status        s ON s.id = a.id AND s.during @> '2024-02-15'::timestamptz;
```

Static attributes need no join — they are columns on `account` itself
(`a.opened_on`), so only the columns that actually change require a temporal
join.

### Options

```
--list FILE        list of CSV paths (required)
--schema NAME      target schema name (default: account)
--key COL          account natural-key column (default: auto-detected)
--enum-max N       max distinct values to treat a column as enum-like (default: 32)
--null-token TOK   value to treat as NULL besides empty string (repeatable)
--lenient          tolerate ragged rows, blank keys, and header drift
                   instead of erroring (default: strict)
--with-checks      bake the inferred profile into the DDL as numeric(p,s)
                   types and CHECK constraints (analyze, load)
-o, --out FILE     write output to a file (analyze, report, rust)
--database-url URL connection string (load; falls back to $DATABASE_URL)
--create-schema    run the CREATE script before loading (load)
```

Runnable examples live in `example/` (a minimal three-month set) and
`testdata/` (a richer four-month set with quoted fields, nulls, a late-
arriving enum value, and an account added and removed) — point `--list` at
either `list.txt`. Tests: `cargo test`.
