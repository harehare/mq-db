//! `EXPLAIN [ANALYZE]` plan description.

use super::engine::SqlEngine;
use super::external::table_factor_ident;
use super::output::QueryOutput;
use super::plan::{
    candidate_hints_for_where, expr_col_name, expr_int_val, expr_str_val, flatten_and_conjuncts,
    zone_map_skip,
};
use super::result_ops::{limit_expr_of, offset_expr_of, unwrap_nested};
use crate::MqdbError;
use crate::block::BlockType;
use crate::indexes::IndexHint;
use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, JoinConstraint, JoinOperator, OrderByKind, OrderBySort,
    Query, Select, SetExpr, Statement,
};

fn describe_hint(hint: &IndexHint) -> String {
    match hint {
        IndexHint::BlockType(types) => format!(
            "BitmapIndex(block_type IN ({}))",
            types
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        IndexHint::PreExact(n) => format!("BTreeIndex(pre = {n})"),
        IndexHint::PreRange(lo, hi) => format!("BTreeIndex(pre BETWEEN {lo} AND {hi})"),
        IndexHint::ContentExact(s) => format!("HashIndex(content = '{s}')"),
        IndexHint::LangExact(s) => format!("HashIndex(lang = '{s}')"),
        IndexHint::DepthExact(d) => format!("HashIndex(depth = {d})"),
        IndexHint::TermMatch(terms) => format!("TermIndex(match: {})", terms.join(", ")),
        IndexHint::FullScan => "full scan".to_string(),
    }
}

fn zone_map_candidate_fields(where_expr: &Expr) -> Vec<&'static str> {
    let mut eq_block_type: Option<BlockType> = None;
    let mut has_content = false;
    let mut fields = Vec::new();

    for conjunct in flatten_and_conjuncts(where_expr) {
        let Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } = conjunct
        else {
            continue;
        };
        let col = expr_col_name(left).or_else(|| expr_col_name(right));
        let val = expr_str_val(right).or_else(|| expr_str_val(left));
        let int_val = expr_int_val(right).or_else(|| expr_int_val(left));

        match col.as_deref() {
            Some("block_type") => {
                if let Some(s) = val.as_deref()
                    && let Some(bt) = BlockType::from_str(s)
                {
                    eq_block_type = Some(bt);
                }
            }
            Some("lang") => {
                if let Some(s) = val
                    && !s.is_empty()
                {
                    fields.push("lang");
                }
            }
            Some("depth") => {
                if let Some(n) = int_val
                    && n > 0
                {
                    fields.push("depth");
                }
            }
            Some("content") => has_content = true,
            _ => {}
        }
    }

    if has_content && eq_block_type == Some(BlockType::Heading) {
        fields.push("heading content");
    }
    fields
}

fn describe_join_strategy(on: &Expr) -> String {
    for conjunct in flatten_and_conjuncts(on) {
        if let Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } = conjunct
            && expr_col_name(left).is_some()
            && expr_col_name(right).is_some()
        {
            return format!("hash join on {left} = {right}");
        }
    }
    "nested loop (cross join + filter)".to_string()
}

impl<'a> SqlEngine<'a> {
    pub(super) fn exec_explain(
        &self,
        analyze: bool,
        inner: &Statement,
    ) -> Result<QueryOutput, MqdbError> {
        let Statement::Query(query) = inner else {
            return Err(MqdbError::SqlExec(
                "EXPLAIN only supports SELECT queries".into(),
            ));
        };

        let mut rows: Vec<(String, String)> = Vec::new();
        self.describe_query(query, "query", &mut Vec::new(), &mut rows);

        if analyze {
            let start = std::time::Instant::now();
            let out = self.exec_query(query)?;
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            rows.push(("actual:elapsed".to_string(), format!("{elapsed_ms:.3}ms")));
            rows.push((
                "actual:rows".to_string(),
                format!("{} row(s) returned", out.rows.len()),
            ));
            self.explain_analyze_scan_stats(query, &mut rows);
        }

        Ok(QueryOutput {
            columns: vec!["step".to_string(), "detail".to_string()],
            rows: rows.into_iter().map(|(s, d)| vec![s, d]).collect(),
        })
    }

    fn describe_query(
        &self,
        query: &Query,
        label: &str,
        cte_names: &mut Vec<String>,
        out: &mut Vec<(String, String)>,
    ) {
        let mut local_ctes = 0;
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                let name = cte.alias.name.value.to_lowercase();
                self.describe_query(&cte.query, &format!("cte:{name}"), cte_names, out);
                cte_names.push(name);
                local_ctes += 1;
            }
        }
        match query.body.as_ref() {
            SetExpr::Select(select) => {
                let limit = limit_expr_of(query);
                let offset = offset_expr_of(query);
                self.describe_select(
                    select,
                    &query.order_by,
                    (limit.as_ref(), offset.as_ref()),
                    label,
                    cte_names,
                    out,
                );
            }
            _ => {
                out.push((
                    label.to_string(),
                    "non-SELECT set expression (VALUES/UNION/...) — no index plan".to_string(),
                ));
            }
        }
        cte_names.truncate(cte_names.len() - local_ctes);
    }

    fn describe_select(
        &self,
        select: &Select,
        order_by: &Option<sqlparser::ast::OrderBy>,
        limit_offset: (Option<&Expr>, Option<&Expr>),
        label: &str,
        cte_names: &[String],
        out: &mut Vec<(String, String)>,
    ) {
        let (limit, offset) = limit_offset;
        if select.from.is_empty() {
            out.push((
                format!("{label}:from"),
                "no FROM clause (constant SELECT)".to_string(),
            ));
        } else {
            let twj = &select.from[0];
            match table_factor_ident(&twj.relation) {
                Some(table_name) => {
                    let is_cte = cte_names.contains(&table_name);
                    let kind = if is_cte {
                        "cte"
                    } else {
                        match table_name.as_str() {
                            "blocks" => "blocks",
                            "documents" => "documents",
                            "links" => "links",
                            other
                                if self.store.custom_tables.read().unwrap().contains_key(other) =>
                            {
                                "custom table"
                            }
                            _ => "unknown",
                        }
                    };
                    out.push((format!("{label}:from"), format!("{table_name} ({kind})")));

                    let single_unjoined_from = select.from.len() == 1 && twj.joins.is_empty();
                    let is_plain_blocks = kind == "blocks";

                    match select.selection.as_ref() {
                        Some(we) if is_plain_blocks => {
                            let candidates = candidate_hints_for_where(we);
                            let hint = self.choose_best_hint(candidates.clone());
                            if candidates.len() <= 1 {
                                out.push((
                                    format!("{label}:where"),
                                    format!("{} used", describe_hint(&hint)),
                                ));
                            } else {
                                let mut costed: Vec<(u64, &IndexHint)> = candidates
                                    .iter()
                                    .map(|h| (self.estimate_hint_cost(h), h))
                                    .collect();
                                costed.sort_by_key(|(cost, _)| *cost);
                                let others: Vec<String> = costed
                                    .iter()
                                    .filter(|(_, h)| **h != hint)
                                    .map(|(cost, h)| format!("{} [est. {cost}]", describe_hint(h)))
                                    .collect();
                                out.push((
                                    format!("{label}:where"),
                                    format!(
                                        "{} used (est. {} row(s); also considered: {})",
                                        describe_hint(&hint),
                                        self.estimate_hint_cost(&hint),
                                        others.join(", ")
                                    ),
                                ));
                            }

                            if single_unjoined_from {
                                let fields = zone_map_candidate_fields(we);
                                if fields.is_empty() {
                                    out.push((
                                        format!("{label}:zone-map"),
                                        "not eligible (no lang=/depth=/heading-content= conjunct)"
                                            .to_string(),
                                    ));
                                } else {
                                    out.push((
                                        format!("{label}:zone-map"),
                                        format!("eligible via {}", fields.join(", ")),
                                    ));
                                }
                            } else {
                                out.push((
                                    format!("{label}:zone-map"),
                                    "disabled (JOIN or multiple FROM tables)".to_string(),
                                ));
                            }

                            let where_fully_indexed = matches!(hint, IndexHint::TermMatch(_))
                                && single_unjoined_from
                                && matches!(unwrap_nested(we), Expr::Function(_));
                            out.push((
                                format!("{label}:where-recheck"),
                                if where_fully_indexed {
                                    "skipped (fully covered by TermIndex match())".to_string()
                                } else {
                                    "row-by-row (full predicate re-evaluated after scan)"
                                        .to_string()
                                },
                            ));
                        }
                        Some(_) => {
                            out.push((
                                format!("{label}:where"),
                                "row-by-row (no secondary index for this table)".to_string(),
                            ));
                        }
                        None => {
                            out.push((format!("{label}:where"), "none — full scan".to_string()));
                        }
                    }
                }
                None => {
                    out.push((
                        format!("{label}:from"),
                        "unsupported FROM clause (subquery/derived table)".to_string(),
                    ));
                }
            }

            for (i, join) in twj.joins.iter().enumerate() {
                let jname = table_factor_ident(&join.relation).unwrap_or_else(|| "?".to_string());
                let strategy = match &join.join_operator {
                    JoinOperator::Inner(JoinConstraint::On(on))
                    | JoinOperator::Join(JoinConstraint::On(on))
                    | JoinOperator::Left(JoinConstraint::On(on))
                    | JoinOperator::LeftOuter(JoinConstraint::On(on)) => describe_join_strategy(on),
                    _ => "cross join (no ON, or unsupported join type)".to_string(),
                };
                out.push((
                    format!("{label}:join[{i}]"),
                    format!("{jname}: {strategy} — join partner always full-scanned"),
                ));
            }

            for twj_extra in select.from.iter().skip(1) {
                let n = table_factor_ident(&twj_extra.relation).unwrap_or_else(|| "?".to_string());
                out.push((
                    format!("{label}:from+"),
                    format!("{n}: cross join (comma-separated FROM)"),
                ));
            }
        }

        let group_by_exprs: Vec<Expr> = match &select.group_by {
            GroupByExpr::Expressions(exprs, _) => exprs.clone(),
            _ => vec![],
        };
        if !group_by_exprs.is_empty() {
            out.push((
                format!("{label}:group-by"),
                format!("{} key(s)", group_by_exprs.len()),
            ));
        }

        if let Some(ob) = order_by
            && let OrderByKind::Expressions(exprs) = &ob.kind
        {
            let desc = exprs
                .iter()
                .map(|e| {
                    format!(
                        "{} {}",
                        e.expr,
                        if e.options.sort == Some(OrderBySort::Desc) {
                            "DESC"
                        } else {
                            "ASC"
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.push((format!("{label}:order-by"), desc));
        }

        if let Some(lim) = limit {
            out.push((format!("{label}:limit"), format!("{lim}")));
        }
        if let Some(off) = offset {
            out.push((format!("{label}:offset"), format!("{off}")));
        }
    }

    fn explain_analyze_scan_stats(&self, query: &Query, out: &mut Vec<(String, String)>) {
        let SetExpr::Select(select) = query.body.as_ref() else {
            return;
        };
        if select.from.len() != 1 || !select.from[0].joins.is_empty() {
            return;
        }
        let Some(table_name) = table_factor_ident(&select.from[0].relation) else {
            return;
        };
        let shadowed_by_cte = query.with.as_ref().is_some_and(|w| {
            w.cte_tables
                .iter()
                .any(|c| c.alias.name.value.eq_ignore_ascii_case("blocks"))
        });
        if table_name != "blocks" || shadowed_by_cte {
            return;
        }

        let where_expr = select.selection.as_ref();
        let hint = where_expr
            .map(|we| self.choose_best_hint(candidate_hints_for_where(we)))
            .unwrap_or(IndexHint::FullScan);

        let (mut docs_total, mut docs_skipped, mut candidate_rows, mut total_rows) =
            (0u32, 0u32, 0u32, 0u32);
        for (doc, doc_idx) in self.documents_with_indexes() {
            docs_total += 1;
            total_rows += doc.blocks.len() as u32;
            if let Some(we) = where_expr
                && zone_map_skip(&doc.zone_maps, we)
            {
                docs_skipped += 1;
                continue;
            }
            candidate_rows += hint
                .resolve(doc_idx)
                .map(|v| v.len() as u32)
                .unwrap_or(doc.blocks.len() as u32);
        }
        out.push((
            "actual".to_string(),
            format!("{docs_skipped}/{docs_total} document(s) skipped by zone map"),
        ));
        out.push((
            "actual".to_string(),
            format!(
                "{candidate_rows} candidate row(s) from index/scan (of {total_rows} total blocks)"
            ),
        ));
    }
}
