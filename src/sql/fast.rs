use super::*;
use sqlparser::ast::{Expr, BinaryOperator, UnaryOperator, SetExpr, SelectItem, TableFactor, GroupByExpr,
    FunctionArguments, FunctionArg, FunctionArgExpr, Value as SqlValue, OrderByKind};
use crate::computed::Computed;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Source { Column(String), Computed(Computed) }

impl Source {
    fn stats(&self, store: &mut Store, table: &str, span: Span) -> Result<Stats> {
        match self {
            Self::Column(column)=>store.aggregate(table,column,span),
            Self::Computed(source)=>store.computed_aggregate(table,source,span,false),
        }
    }
    fn ranges(&self, store: &mut Store, table: &str, ranges: &[(i64,i64)]) -> Result<Vec<Stats>> {
        match self {
            Self::Column(column)=>store.stats_ranges(table,column,ranges),
            Self::Computed(source)=>store.computed_ranges(table,source,ranges,false),
        }
    }
    fn kind(&self, store: &Store, table: &str) -> Result<Kind> {
        match self {
            Self::Column(column)=>Ok(store.columns(table)?.iter().find(|c|c.name==*column).context("no such column")?.kind),
            Self::Computed(_)=>Ok(Kind::Real),
        }
    }
}

fn source(store: &Store, table: &str, expr: &Expr) -> Result<Option<Source>> {
    let columns = store.columns(table)?;
    let column = |expr:&Expr|identifier(expr).and_then(|name|columns.iter().find(|c|c.name.eq_ignore_ascii_case(name))).map(|c|c.name.clone());
    if let Some(column) = column(expr) { return Ok(Some(Source::Column(column))); }
    let Expr::Function(f) = expr else { return Ok(None) };
    if f.over.is_some() || f.filter.is_some() || !f.within_group.is_empty() { return Ok(None); }
    let name = f.name.to_string().to_ascii_lowercase();
    let Some((name,suffix)) = name.rsplit_once('_') else { return Ok(None) };
    let unit_system = match suffix { "c"=>17,"f"=>1,_=>return Ok(None) };
    let Ok(formula) = name.parse() else { return Ok(None) };
    let FunctionArguments::List(args) = &f.args else { return Ok(None) };
    if args.args.len()!=2 || args.duplicate_treatment.is_some() || !args.clauses.is_empty() { return Ok(None); }
    let mut inputs = Vec::new();
    for arg in &args.args {
        let FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) = arg else { return Ok(None) };
        let Some(column) = column(expr) else { return Ok(None) };
        inputs.push(column);
    }
    Ok(Some(Source::Computed(Computed {formula,temperature:inputs[0].clone(),humidity:inputs[1].clone(),unit_system})))
}

pub(super) fn query(store: &mut Store, statement: &Statement, parameters: &[Cell]) -> Result<Option<Vec<Vec<Cell>>>> {
    if let Some(row) = aggregate(store,statement,parameters)? { return Ok(Some(vec![row])); }
    grouped(store,statement,parameters)
}

fn grouped(store: &mut Store, statement: &Statement, parameters: &[Cell]) -> Result<Option<Vec<Vec<Cell>>>> {
    let Statement::Query(query) = statement else { return Ok(None) };
    let SetExpr::Select(select) = query.body.as_ref() else { return Ok(None) };
    if query.with.is_some() || query.limit_clause.is_some() || select.from.len() != 1
        || !select.from[0].joins.is_empty() || select.having.is_some() || select.distinct.is_some()
        || !select.named_window.is_empty() || select.qualify.is_some() { return Ok(None); }
    let GroupByExpr::Expressions(groups,modifiers) = &select.group_by else { return Ok(None) };
    if groups.len() != 1 || !modifiers.is_empty() { return Ok(None); }
    let TableFactor::Table {name,args:None,..} = &select.from[0].relation else { return Ok(None) };
    let table = vtab::dequote(&name.to_string()).into_owned();
    if store.table_index(&table).is_err() { return Ok(None); }
    let table = store.snapshot.manifest.tables[store.table_index(&table)?].name.clone();
    let mut expressions = Vec::new();
    let mut key = None;
    for (i,item) in select.projection.iter().enumerate() {
        let expr = match item { SelectItem::UnnamedExpr(e)|SelectItem::ExprWithAlias {expr:e,..}=>e,_=>return Ok(None) };
        if let Expr::Function(f) = expr && f.name.to_string().eq_ignore_ascii_case("time_bucket") && f.over.is_none() {
            let FunctionArguments::List(args) = &f.args else { return Ok(None) };
            if args.args.len() != 2 || args.duplicate_treatment.is_some() || !args.clauses.is_empty() { return Ok(None); }
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(time)) = &args.args[0] else { return Ok(None) };
            let FunctionArg::Unnamed(FunctionArgExpr::Expr(width)) = &args.args[1] else { return Ok(None) };
            let Some(width) = integer(width,&[],&mut 0).filter(|n|*n > 0) else { return Ok(None) };
            if !identifier(time).is_some_and(|n|n.eq_ignore_ascii_case("dateTime")) || key.is_some() || f.filter.is_some() || !f.within_group.is_empty() { return Ok(None); }
            key = Some((i,width));
        }
        expressions.push(expr);
    }
    let Some((key_index,width)) = key else { return Ok(None) };
    let matches_key = |expr: &Expr| {
        expr == expressions[key_index] || integer(expr,&[],&mut 0) == Some(key_index as i64+1)
            || matches!(&select.projection[key_index],SelectItem::ExprWithAlias {alias,..} if identifier(expr)==Some(alias.value.as_str()))
    };
    if !matches_key(&groups[0]) { return Ok(None); }
    // SQLite resolves an ambiguous GROUP BY identifier to an input column
    // before a SELECT alias. A sensor named like the alias must keep that rule.
    if let Expr::Identifier(id) = &groups[0]
        && store.columns(&table)?.iter().any(|c|c.name.eq_ignore_ascii_case(&id.value)) { return Ok(None); }
    let mut descending = false;
    if let Some(order) = &query.order_by {
        let OrderByKind::Expressions(orders) = &order.kind else { return Ok(None) };
        if orders.len() != 1 || !matches_key(&orders[0].expr) || orders[0].with_fill.is_some() { return Ok(None); }
        descending = orders[0].options.asc == Some(false);
    }
    let mut lower = None; let mut upper = None; let mut positional = 0;
    if let Some(selection) = &select.selection && !bounds(selection,parameters,&mut positional,&mut lower,&mut upper) { return Ok(None); }
    let start = match lower {None=>i64::MIN,Some((t,false))=>t,Some((t,true))=>match t.checked_sub(1) {Some(t)=>t,None=>return Ok(None)}};
    let stop = match upper {None=>i64::MAX,Some((t,true))=>t,Some((t,false))=>match t.checked_sub(1) {Some(t)=>t,None=>return Ok(None)}};
    if start >= stop { return Ok(Some(vec![])); }
    let time = store.aggregate(&table,"dateTime",Span {start,stop,daily:false})?;
    let (Some(min),Some(max)) = (time.min,time.max) else { return Ok(Some(vec![])) };
    if min.abs().max(max.abs()) >= (1_u64<<53) as f64 { return Ok(None); }
    let first = (min as i128).div_euclid(width as i128)*width as i128;
    let last = (max as i128).div_euclid(width as i128)*width as i128;
    if first <= i64::MIN as i128 || last+width as i128 > i64::MAX as i128 { return Ok(None); }
    let count = usize::try_from((last-first)/width as i128+1)?;
    let mut columns = HashMap::new();
    for (i,expr) in expressions.iter().enumerate() {
        if i != key_index && reduce(store,&table,Span {start:0,stop:0,daily:false},expr,&mut columns)?.is_none() { return Ok(None); }
    }
    let per_bin = (columns.len()+1)*std::mem::size_of::<Stats>() + expressions.len()*std::mem::size_of::<Cell>() + 160;
    ensure!(count <= store.workspace_bytes/per_bin,"group workspace exceeded; increase workspace_mib or restrict the time span");
    let ranges = (0..count).map(|i| {
        let key = first+i as i128*width as i128;
        ((key-1).max(start as i128) as i64,(key+width as i128-1).min(stop as i128) as i64)
    }).collect::<Vec<_>>();
    let occupied = store.stats_ranges(&table,"dateTime",&ranges)?;
    let mut series = HashMap::new();
    for source in columns.keys() { series.insert(source.clone(),source.ranges(store,&table,&ranges)?); }
    let mut rows = Vec::new();
    for (i,time) in occupied.iter().enumerate() {
        if time.count == 0 { continue; }
        let mut cached = series.iter().map(|(column,values)|(column.clone(),values[i].clone())).collect();
        let mut row = Vec::new();
        for (ci,expr) in expressions.iter().enumerate() {
            if ci == key_index { row.push(Cell::Integer((first+i as i128*width as i128) as i64)); }
            else if let Some(value) = reduce(store,&table,Span {start:0,stop:0,daily:false},expr,&mut cached)? { row.push(value); }
            else { return Ok(None); }
        }
        rows.push(row);
    }
    if descending { rows.reverse(); }
    Ok(Some(rows))
}

pub(super) fn aggregate(store: &mut Store, statement: &Statement, parameters: &[Cell]) -> Result<Option<Vec<Cell>>> {
    let Statement::Query(query) = statement else { return Ok(None) };
    let SetExpr::Select(select) = query.body.as_ref() else { return Ok(None) };
    if query.with.is_some() || query.limit_clause.is_some() || select.from.len() != 1
        || !select.from[0].joins.is_empty() || select.having.is_some() || select.distinct.is_some()
        || !select.named_window.is_empty() || select.qualify.is_some()
        || !matches!(&select.group_by, GroupByExpr::Expressions(e, _) if e.is_empty()) { return Ok(None); }
    let TableFactor::Table { name, args: None, .. } = &select.from[0].relation else { return Ok(None) };
    let name = vtab::dequote(&name.to_string()).into_owned();
    if store.table_index(&name).is_err() { return Ok(None); }
    let ti = store.table_index(&name)?;
    let name = store.snapshot.manifest.tables[ti].name.clone();
    if store.snapshot.manifest.tables[ti].blocks.first().is_some_and(|b|b.first == i64::MIN)
        || store.changes.get(&name).is_some_and(|c|c.contains_key(&i64::MIN)) { return Ok(None); }
    let mut lower = None;
    let mut upper = None;
    let mut positional = 0;
    if let Some(selection) = &select.selection
        && !bounds(selection, parameters, &mut positional, &mut lower, &mut upper) { return Ok(None); }
    let start = match lower { None => i64::MIN, Some((t,false)) => t,
        Some((t,true)) => match t.checked_sub(1) { Some(t) => t, None => return Ok(None) } };
    let stop = match upper { None => i64::MAX, Some((t,true)) => t,
        Some((t,false)) => match t.checked_sub(1) { Some(t) => t, None => return Ok(None) } };
    let span = Span { start, stop: stop.max(start), daily: false };
    let mut cached = HashMap::new();
    let mut result = Vec::new();
    for item in &select.projection {
        let expr = match item { SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr:e, .. } => e, _ => return Ok(None) };
        let Some(value) = reduce(store, &name, span, expr, &mut cached)? else { return Ok(None) };
        result.push(value);
    }
    Ok((!cached.is_empty()).then_some(result))
}

fn identifier(expr: &Expr) -> Option<&str> {
    match expr { Expr::Identifier(i) => Some(&i.value), Expr::CompoundIdentifier(i) => i.last().map(|i| i.value.as_str()), _ => None }
}

fn integer(expr: &Expr, parameters: &[Cell], positional: &mut usize) -> Option<i64> {
    match expr {
        Expr::Value(v) => match &v.value {
            SqlValue::Number(n, _) => n.parse().ok(),
            SqlValue::Placeholder(p) => {
                let i = if p == "?" { let i = *positional; *positional += 1; i } else { p.strip_prefix('?')?.parse::<usize>().ok()?.checked_sub(1)? };
                match parameters.get(i)? { Cell::Integer(n) => Some(*n), _ => None }
            }
            _ => None,
        },
        Expr::UnaryOp { op: UnaryOperator::Minus, expr } => integer(expr, parameters, positional)?.checked_neg(),
        Expr::Nested(expr) => integer(expr, parameters, positional), _ => None,
    }
}

fn bounds(expr: &Expr, parameters: &[Cell], positional: &mut usize, lower: &mut Option<(i64,bool)>, upper: &mut Option<(i64,bool)>) -> bool {
    let Expr::BinaryOp { left, op, right } = expr else { if let Expr::Nested(e) = expr { return bounds(e, parameters, positional, lower, upper); } return false; };
    if *op == BinaryOperator::And { return bounds(left, parameters, positional, lower, upper) && bounds(right, parameters, positional, lower, upper); }
    if !identifier(left).is_some_and(|n|n.eq_ignore_ascii_case("dateTime")) { return false; }
    let Some(time) = integer(right, parameters, positional) else { return false };
    match op {
        BinaryOperator::Gt => tighten_lower(lower,time,false), BinaryOperator::GtEq => tighten_lower(lower,time,true),
        BinaryOperator::Lt => tighten_upper(upper,time,false), BinaryOperator::LtEq => tighten_upper(upper,time,true),
        BinaryOperator::Eq => { tighten_lower(lower,time,true); tighten_upper(upper,time,true); }, _ => return false,
    }
    true
}

fn reduce(store: &mut Store, table: &str, span: Span, expr: &Expr, cached: &mut HashMap<Source,Stats>) -> Result<Option<Cell>> {
    Ok(match expr {
        Expr::Nested(e) => reduce(store,table,span,e,cached)?,
        Expr::Value(v) => match &v.value { SqlValue::Number(n,_) => Some(if let Ok(n) = n.parse::<i64>() { Cell::Integer(n) } else { Cell::Real(n.parse::<f64>()?.to_bits()) }), _ => None },
        Expr::BinaryOp { left, op, right } => {
            let Some(a) = reduce(store,table,span,left,cached)? else { return Ok(None) };
            let Some(b) = reduce(store,table,span,right,cached)? else { return Ok(None) };
            if let (Cell::Integer(a), Cell::Integer(b)) = (&a,&b) {
                let value = match op {
                    BinaryOperator::Plus => a.checked_add(*b), BinaryOperator::Minus => a.checked_sub(*b),
                    BinaryOperator::Multiply => a.checked_mul(*b), BinaryOperator::Divide if *b != 0 => a.checked_div(*b),
                    BinaryOperator::Divide => return Ok(Some(Cell::Null)), _ => return Ok(None),
                };
                return Ok(value.map(Cell::Integer));
            }
            match (a.numeric(), b.numeric()) {
                (Some(a),Some(b)) => Some(Cell::Real(match op {
                    BinaryOperator::Plus => a+b, BinaryOperator::Minus => a-b,
                    BinaryOperator::Multiply => a*b, BinaryOperator::Divide if b != 0.0 => a/b,
                    BinaryOperator::Divide => return Ok(Some(Cell::Null)), _ => return Ok(None),
                }.to_bits())), _ => Some(Cell::Null),
            }
        }
        Expr::Function(function) if function.over.is_none() && function.filter.is_none() && function.within_group.is_empty() => {
            let FunctionArguments::List(arguments) = &function.args else { return Ok(None) };
            if arguments.args.len() != 1 || arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() { return Ok(None); }
            let name = function.name.to_string().to_ascii_lowercase();
            if !["count","sum","avg","min","max"].contains(&name.as_str()) { return Ok(None); }
            let source = match &arguments.args[0] {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => source(store,table,e)?,
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard) if name == "count" => Some(Source::Column("dateTime".to_owned())), _ => None,
            };
            let Some(source) = source else { return Ok(None) };
            if !cached.contains_key(&source) {
                let stats = if span.start >= span.stop { Stats::default() } else { source.stats(store,table,span)? };
                cached.insert(source.clone(), stats);
            }
            let stats = &cached[&source];
            let kind = source.kind(store,table)?;
            if kind == Kind::Integer && name != "count" && [stats.min,stats.max,stats.sum].into_iter().flatten().any(|n| n.abs() >= (1_u64 << 53) as f64) { return Ok(None); }
            Some(match name.as_str() {
                "count" => Cell::Integer(i64::try_from(stats.count)?),
                "avg" => stats.sum.map(|s| Cell::Real((s / stats.count as f64).to_bits())).unwrap_or(Cell::Null),
                _ => {
                    let value = match name.as_str() { "sum" => stats.sum, "min" => stats.min, _ => stats.max };
                    value.map(|n| if kind == Kind::Integer { Cell::Integer(n as i64) } else { Cell::Real(n.to_bits()) }).unwrap_or(Cell::Null)
                }
            })
        }
        _ => None,
    })
}
