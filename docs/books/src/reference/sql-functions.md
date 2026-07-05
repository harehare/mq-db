# Built-in Functions

## mq-db-specific

| Function | Description |
| --- | --- |
| `under(pre, post, anc_pre, anc_post)` | `O(1)` interval ancestor check — see [Index Layers](index-layers.md) |
| `mq(program, content)` | Run an mq program against Markdown content |
| `json_extract(json, path)` | Extract a value from a JSON string |

```sql
-- Hierarchy query: everything nested under a heading
SELECT b.block_type, b.content
FROM blocks b
WHERE under(b.pre, b.post,
  (SELECT pre FROM blocks WHERE block_type = 'heading' AND content = 'Architecture'),
  (SELECT post FROM blocks WHERE block_type = 'heading' AND content = 'Architecture'));

-- Run an mq program inline against block content
SELECT mq('.h1 | to_text', content) AS title
FROM blocks WHERE block_type = 'code' AND lang = 'markdown';
```

## String

| Function | Description |
| --- | --- |
| `lower` / `upper` | Case conversion |
| `length` / `len` / `char_length` / `character_length` | Character count |
| `trim` / `ltrim` / `rtrim` | Strip whitespace, or the given characters |
| `concat` / `concat_ws` | Join strings (with optional separator) |
| `replace` | Replace all occurrences of a substring |
| `substring` / `substr` | Extract a substring (1-based, `FROM`/`FOR` or comma form) |
| `position` / `instr` | Find the 1-based index of a substring (`0` if absent) |
| `left` / `right` | First/last `n` characters |
| `lpad` / `rpad` | Pad to a fixed length |
| `reverse` | Reverse a string |
| `repeat` | Repeat a string `n` times |
| `initcap` | Capitalize each word |
| `ascii` / `chr` | Char ↔ code point |
| `split_part` | Extract the nth delimiter-separated field |

## Numeric

| Function | Description |
| --- | --- |
| `abs` | Absolute value |
| `round` / `trunc` / `truncate` | Round / truncate, with optional decimal scale |
| `ceil` / `ceiling` / `floor` | Round up / down |
| `mod` | Remainder |
| `power` / `pow` / `sqrt` | Exponentiation / square root |
| `exp` / `ln` | `e^x` / natural log |
| `log` / `log10` / `log2` | Logarithm (1-arg = base 10, 2-arg = custom base) |
| `sign` | `-1` / `0` / `1` |
| `pi` | π |
| `greatest` / `least` | Max / min across arguments (ignoring `NULL`) |

## Date/Time

| Function | Description |
| --- | --- |
| `now` / `current_timestamp` | Current UTC date and time |
| `current_date` | Current UTC date |
| `current_time` | Current UTC time |

## Null handling & control flow

| Function | Description |
| --- | --- |
| `coalesce` / `ifnull` | First non-`NULL` argument |
| `nullif` | `NULL` if the two arguments are equal |
| `CASE WHEN … THEN … ELSE … END` | Conditional expressions |
| `typeof` | Runtime type of a value |

## Aggregates

Usable with `GROUP BY`:

| Function | Description |
| --- | --- |
| `count(*)` / `count(DISTINCT col)` | Row / distinct-value count |
| `min` / `max` / `sum` / `avg` | Standard aggregates |
| `group_concat` / `string_agg(expr[, sep])` | Concatenate group values (default separator `,`) |
