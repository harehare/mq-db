# CTEs (`WITH` / `WITH RECURSIVE`)

## `WITH`

Name an intermediate result and reuse it in the main query, a join, or a subquery:

```sql
WITH headings AS (SELECT content, pre, post FROM blocks WHERE block_type = 'heading')
SELECT content FROM headings WHERE pre < 10 ORDER BY pre;
```

A CTE's (and a [view](sql-ddl.md#views)'s) output columns are recovered from their rendered text, so arithmetic/numeric comparisons on them (`WHERE pre < 10` above) work as expected, but this is a heuristic, not a real type system: a value that merely *looks* numeric (e.g. a text cell containing `"007"`) round-trips as a number, and a float's trailing zeros aren't preserved.

## `WITH RECURSIVE`

The standard `<anchor> UNION [ALL] <recursive term>` shape, evaluated by iterative fixed-point: the anchor runs once, then the recursive term re-runs against only the *previous iteration's new rows* until it produces none. `UNION` (not `ALL`) dedupes against every row produced so far, which is also what makes a query that would otherwise cycle forever terminate. A hard cap of 10,000 iterations guards against a recursive term with no terminating condition. The recursive term must reference the CTE by name exactly once in its `FROM`; the anchor must not reference it at all.

```sql
-- Number sequence: the canonical minimal recursive CTE
WITH RECURSIVE seq AS (
  SELECT 1 AS n
  UNION ALL
  SELECT n + 1 FROM seq WHERE n < 10
)
SELECT n FROM seq ORDER BY n;

-- Heading ancestor chain via interval containment: walk up from a block's
-- pre/post to every heading whose interval contains it
WITH RECURSIVE ancestors AS (
  SELECT pre, post, content FROM blocks WHERE pre = 24
  UNION
  SELECT b.pre, b.post, b.content
  FROM blocks b, ancestors
  WHERE b.pre < ancestors.pre AND ancestors.post < b.post
    AND b.block_type = 'heading'
)
SELECT content FROM ancestors ORDER BY pre;
```

A plain top-level `SELECT ... UNION SELECT ...` outside a recursive CTE is not supported. A CTE name identical to `blocks`, `documents`, or a custom table shadows it for the duration of the `WITH` clause's scope.
