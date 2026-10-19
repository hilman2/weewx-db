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
            Self::Column(column)=>Ok(store.column_defs(table)?.iter().find(|c|c.name==*column).context("no such column")?.kind),
            Self::Computed(_)=>Ok(Kind::Real),
        }
    }
}

fn source(store: &Store, table: &str, expr: &Expr) -> Result<Option<Source>> {
    let columns = store.column_defs(table)?;
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
    if let Some(rows) = ordered(store,statement,parameters)? { return Ok(Some(rows)); }
    if let Some(row) = aggregate(store,statement,parameters)? { return Ok(Some(vec![row])); }
    grouped(store,statement,parameters)
}

/// Answer `SELECT ... FROM table [WHERE ...] ORDER BY expression [, dateTime]
/// LIMIT n [OFFSET m]` when the sort key is computed from columns, such as
/// the warmest mean day `wsum/sumtime`. SQLite cannot pass such an order to
/// the virtual table and would deliver and sort every row.
fn ordered(store: &mut Store, statement: &Statement, parameters: &[Cell]) -> Result<Option<Vec<Vec<Cell>>>> {
    use sqlparser::ast::LimitClause;
    let Statement::Query(query) = statement else { return Ok(None) };
    let SetExpr::Select(select) = query.body.as_ref() else { return Ok(None) };
    if query.with.is_some() || select.from.len() != 1 || !select.from[0].joins.is_empty() || select.having.is_some()
        || select.distinct.is_some() || !select.named_window.is_empty() || select.qualify.is_some()
        || !matches!(&select.group_by, GroupByExpr::Expressions(e, _) if e.is_empty()) { return Ok(None); }
    let TableFactor::Table { name, args: None, .. } = &select.from[0].relation else { return Ok(None) };
    let Ok(ti) = store.table_index(&vtab::dequote(&name.to_string())) else { return Ok(None) };
    let table = store.snapshot.manifest.tables[ti].name.clone();
    let columns = store.snapshot.manifest.tables[ti].columns.clone();
    let ts = columns.iter().position(|c| c.name == "dateTime").context("dateTime missing")?;
    let Some(order) = &query.order_by else { return Ok(None) };
    let OrderByKind::Expressions(orders) = &order.kind else { return Ok(None) };
    if orders.is_empty() || orders.len() > 2 || orders.iter().any(|o| o.with_fill.is_some() || o.options.nulls_first.is_some()) { return Ok(None); }
    // Projection and ORDER BY take no parameters; WHERE, LIMIT and OFFSET take
    // them in this text order.
    let mut constant = Binder { parameters: None, positional: 0 };
    let mut projection = Vec::new();
    let mut aliases = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard(_) => for ci in 0..columns.len() { projection.push(Term::Column(ci)); aliases.push(None); },
            SelectItem::UnnamedExpr(expr) => { let Some(term) = Term::compile(expr, &columns, &mut constant) else { return Ok(None) }; projection.push(term); aliases.push(None); }
            SelectItem::ExprWithAlias { expr, alias } => {
                let Some(term) = Term::compile(expr, &columns, &mut constant) else { return Ok(None) };
                projection.push(term); aliases.push(Some((alias.value.clone(), expr)));
            }
            _ => return Ok(None),
        }
    }
    // ORDER BY resolves a result alias or a result number before a column.
    let key_expr = &orders[0].expr;
    let key = if let Some(position) = integer(key_expr, &[], &mut 0).filter(|n| *n >= 1) {
        let Some(Some((_, expr))) = aliases.get(position as usize - 1) else { return Ok(None) };
        Term::compile(expr, &columns, &mut constant)
    } else if let Some((_, expr)) = identifier(key_expr).and_then(|name| aliases.iter().flatten().find(|(alias, _)| alias.eq_ignore_ascii_case(name))) {
        Term::compile(expr, &columns, &mut constant)
    } else { Term::compile(key_expr, &columns, &mut constant) };
    // A plain column order is answered faster by the virtual table, which
    // can skip whole blocks by their summaries.
    let Some(key) = key.filter(|k| !matches!(k, Term::Column(_))) else { return Ok(None) };
    let descending = orders[0].options.asc == Some(false);
    let later_first = match orders.get(1) {
        None => false,
        Some(o) if identifier(&o.expr).is_some_and(|n| n.eq_ignore_ascii_case("dateTime")) => o.options.asc == Some(false),
        Some(_) => return Ok(None),
    };
    let mut binder = Binder { parameters: Some(parameters), positional: 0 };
    let mut filters = Vec::new();
    if let Some(selection) = &select.selection && !conditions(selection, &columns, &mut binder, &mut filters) { return Ok(None); }
    let (limit, offset) = match &query.limit_clause {
        Some(LimitClause::LimitOffset { limit: Some(limit), offset, limit_by }) if limit_by.is_empty() => (limit, offset.as_ref().map(|o| &o.value)),
        Some(LimitClause::OffsetCommaLimit { offset, limit }) => (limit, Some(offset)),
        _ => return Ok(None),
    };
    let mut count = |expr: &Expr| match Term::compile(expr, &columns, &mut binder) {
        Some(Term::Constant(Number::Integer(n))) if n >= 0 => Some(n as usize), _ => None,
    };
    let Some(limit) = count(limit) else { return Ok(None) };
    let offset = match offset { Some(expr) => match count(expr) { Some(n) => n, None => return Ok(None) }, None => 0 };
    if binder.positional != parameters.len() && parameters.len() > binder.positional { return Ok(None); }
    let keep = limit.saturating_add(offset);
    let width = projection.len();
    ensure!(keep.saturating_mul(width + 2).saturating_mul(std::mem::size_of::<Number>()) <= store.workspace_bytes, "ORDER BY workspace exceeded; increase workspace_mib or reduce LIMIT");
    let (mut lower, mut upper) = (None, None);
    for filter in &filters {
        if let Condition::Compare(Term::Column(ci), op, Term::Constant(Number::Integer(time))) = filter && *ci == ts {
            match op {
                BinaryOperator::Gt => tighten_lower(&mut lower, *time, false), BinaryOperator::GtEq => tighten_lower(&mut lower, *time, true),
                BinaryOperator::Lt => tighten_upper(&mut upper, *time, false), BinaryOperator::LtEq => tighten_upper(&mut upper, *time, true),
                BinaryOperator::Eq => { tighten_lower(&mut lower, *time, true); tighten_upper(&mut upper, *time, true); }
                _ => (),
            }
        }
    }
    let mut used = Vec::new();
    key.columns(&mut used);
    for term in &projection { term.columns(&mut used); }
    for filter in &filters { match filter { Condition::Compare(a, _, b) => { a.columns(&mut used); b.columns(&mut used); }, Condition::Null(a, _) => a.columns(&mut used) } }
    let rank_key = |a: (Number, i64), b: (Number, i64)| {
        let order = a.0.order(b.0);
        let order = if descending { order.reverse() } else { order };
        order.then(if later_first { b.1.cmp(&a.1) } else { a.1.cmp(&b.1) })
    };
    let rank = |a: &(Number, i64, Vec<Number>), b: &(Number, i64, Vec<Number>)| rank_key((a.0, a.1), (b.0, b.1));
    // The last kept candidate after the latest truncation. A row ranking
    // after it cannot enter the result.
    let mut worst: Option<(Number, i64)> = None;
    let mut best: Vec<(Number, i64, Vec<Number>)> = Vec::new();
    let mut supported = true;
    if keep > 0 {
        let mut row = vec![Number::Null; columns.len()];
        store.visit_rows(&table, &used, (lower, upper), |time, values| {
            for (&ci, value) in used.iter().zip(values) {
                let Some(value) = Number::of(value) else { supported = false; return Ok(false) };
                row[ci] = value;
            }
            if filters.iter().all(|f| f.holds(&row)) {
                let value = key.evaluate(&row);
                if worst.is_none_or(|w| rank_key((value, time), w).is_lt()) {
                    best.push((value, time, projection.iter().map(|t| t.evaluate(&row)).collect()));
                    if best.len() >= keep.saturating_mul(2).max(64) {
                        best.sort_by(rank); best.truncate(keep);
                        worst = best.last().map(|b| (b.0, b.1));
                    }
                }
            }
            Ok(true)
        })?;
    }
    if !supported { return Ok(None); }
    best.sort_by(rank);
    Ok(Some(best.into_iter().skip(offset).take(limit).map(|(_, _, values)| values.into_iter().map(Number::cell).collect()).collect()))
}

/// Whether `query` can possibly answer this statement: it has an aggregate
/// function, a GROUP BY, or an ORDER BY on a computed expression with LIMIT.
/// Statements without any of them skip the native attempt entirely.
pub(super) fn candidate(statement: &Statement) -> bool {
    let Statement::Query(query) = statement else { return false };
    let SetExpr::Select(select) = query.body.as_ref() else { return false };
    if !matches!(&select.group_by, GroupByExpr::Expressions(e, _) if e.is_empty()) { return true; }
    let function = |expr: &Expr| matches!(expr, Expr::Function(_));
    if select.projection.iter().any(|item| matches!(item, SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } if contains(e, &function))) {
        return true;
    }
    // An alias of a computed result column counts as a computed order.
    let alias = |name: &str| select.projection.iter().any(|item| matches!(item, SelectItem::ExprWithAlias { alias, expr }
        if alias.value.eq_ignore_ascii_case(name) && !matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_))));
    let computed = query.order_by.as_ref().is_some_and(|order| matches!(&order.kind, OrderByKind::Expressions(orders)
        if orders.first().is_some_and(|o| match &o.expr {
            Expr::Identifier(name) => alias(&name.value),
            Expr::CompoundIdentifier(_) | Expr::Value(_) => false,
            _ => true,
        })));
    computed && query.limit_clause.is_some()
}

fn contains(expr: &Expr, test: &dyn Fn(&Expr) -> bool) -> bool {
    test(expr) || match expr {
        Expr::BinaryOp { left, right, .. } => contains(left, test) || contains(right, test),
        Expr::UnaryOp { expr, .. } | Expr::Nested(expr) => contains(expr, test),
        _ => false,
    }
}

/// A number as SQLite computes with it.
#[derive(Clone, Copy, Debug)]
enum Number { Null, Integer(i64), Real(f64) }

impl Number {
    fn of(cell: &Cell) -> Option<Self> {
        match cell { Cell::Null => Some(Self::Null), Cell::Integer(n) => Some(Self::Integer(*n)),
            Cell::Real(bits) => Some(Self::Real(f64::from_bits(*bits))), _ => None }
    }
    fn cell(self) -> Cell {
        match self { Self::Null => Cell::Null, Self::Integer(n) => Cell::Integer(n), Self::Real(n) => Cell::Real(n.to_bits()) }
    }
    fn real(self) -> Option<f64> {
        match self { Self::Null => None, Self::Integer(n) => Some(n as f64), Self::Real(n) => Some(n) }
    }
    /// Order with NULL lowest; integers and reals compare exactly.
    fn order(self, other: Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Self::Null, Self::Null) => Ordering::Equal, (Self::Null, _) => Ordering::Less, (_, Self::Null) => Ordering::Greater,
            (Self::Integer(a), Self::Integer(b)) => a.cmp(&b),
            (Self::Real(a), Self::Real(b)) => a.total_cmp(&b),
            (Self::Integer(a), Self::Real(b)) => integer_real(a, b),
            (Self::Real(a), Self::Integer(b)) => integer_real(b, a).reverse(),
        }
    }
}

fn integer_real(a: i64, b: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if b < -9223372036854775808.0 { return Ordering::Greater; }
    if b >= 9223372036854775808.0 { return Ordering::Less; }
    let whole = b.floor();
    a.cmp(&(whole as i64)).then(if b > whole { Ordering::Less } else { Ordering::Equal })
}

#[derive(Clone, Copy)]
enum Operator { Add, Subtract, Multiply, Divide }

/// Statement parameters for placeholders, consumed in text order. Without
/// parameters, a placeholder makes the expression unsupported.
struct Binder<'a> { parameters: Option<&'a [Cell]>, positional: usize }

impl Binder<'_> {
    fn value(&mut self, placeholder: &str) -> Option<Number> {
        let parameters = self.parameters?;
        let index = if placeholder == "?" { let i = self.positional; self.positional += 1; i }
            else { placeholder.strip_prefix('?')?.parse::<usize>().ok()?.checked_sub(1)? };
        Number::of(parameters.get(index)?)
    }
}

/// Arithmetic on table columns and constants, evaluated with SQLite's rules.
enum Term { Column(usize), Constant(Number), Negate(Box<Term>), Binary(Operator, Box<Term>, Box<Term>) }

impl Term {
    fn compile(expr: &Expr, columns: &[Column], binder: &mut Binder) -> Option<Self> {
        Some(match expr {
            Expr::Nested(expr) => Self::compile(expr, columns, binder)?,
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                let name = identifier(expr)?;
                Self::Column(columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))?)
            }
            Expr::Value(value) => match &value.value {
                SqlValue::Number(n, _) => Self::Constant(match n.parse::<i64>() { Ok(n) => Number::Integer(n), Err(_) => Number::Real(n.parse().ok()?) }),
                SqlValue::Null => Self::Constant(Number::Null),
                SqlValue::Placeholder(p) => Self::Constant(binder.value(p)?),
                _ => return None,
            },
            Expr::UnaryOp { op: UnaryOperator::Minus, expr } => Self::Negate(Box::new(Self::compile(expr, columns, binder)?)),
            Expr::UnaryOp { op: UnaryOperator::Plus, expr } => Self::compile(expr, columns, binder)?,
            Expr::BinaryOp { left, op, right } => {
                let operator = match op { BinaryOperator::Plus => Operator::Add, BinaryOperator::Minus => Operator::Subtract,
                    BinaryOperator::Multiply => Operator::Multiply, BinaryOperator::Divide => Operator::Divide, _ => return None };
                let left = Self::compile(left, columns, binder)?;
                let right = Self::compile(right, columns, binder)?;
                Self::Binary(operator, Box::new(left), Box::new(right))
            }
            _ => return None,
        })
    }

    fn columns(&self, used: &mut Vec<usize>) {
        match self {
            Self::Column(ci) => if !used.contains(ci) { used.push(*ci); },
            Self::Constant(_) => (),
            Self::Negate(term) => term.columns(used),
            Self::Binary(_, left, right) => { left.columns(used); right.columns(used); }
        }
    }

    fn evaluate(&self, row: &[Number]) -> Number {
        match self {
            Self::Column(ci) => row[*ci],
            Self::Constant(value) => *value,
            Self::Negate(term) => match term.evaluate(row) {
                Number::Integer(n) => n.checked_neg().map_or(Number::Real(-(n as f64)), Number::Integer),
                Number::Real(n) => Number::Real(-n), Number::Null => Number::Null,
            },
            Self::Binary(operator, left, right) => {
                let (a, b) = (left.evaluate(row), right.evaluate(row));
                if let (Number::Integer(a), Number::Integer(b)) = (a, b) {
                    // Integer overflow continues in floating point.
                    let exact = match operator {
                        Operator::Add => a.checked_add(b), Operator::Subtract => a.checked_sub(b),
                        Operator::Multiply => a.checked_mul(b),
                        Operator::Divide => if b == 0 { return Number::Null } else { a.checked_div(b) },
                    };
                    if let Some(value) = exact { return Number::Integer(value); }
                }
                let (Some(a), Some(b)) = (a.real(), b.real()) else { return Number::Null };
                match operator {
                    Operator::Add => Number::Real(a + b), Operator::Subtract => Number::Real(a - b),
                    Operator::Multiply => Number::Real(a * b),
                    Operator::Divide => if b == 0.0 { Number::Null } else { Number::Real(a / b) },
                }
            }
        }
    }
}

/// A WHERE term: a comparison of two terms, or a NULL test.
enum Condition { Compare(Term, BinaryOperator, Term), Null(Term, bool) }

impl Condition {
    fn holds(&self, row: &[Number]) -> bool {
        match self {
            Self::Null(term, null) => matches!(term.evaluate(row), Number::Null) == *null,
            Self::Compare(left, op, right) => {
                let (a, b) = (left.evaluate(row), right.evaluate(row));
                if matches!(a, Number::Null) || matches!(b, Number::Null) { return false; }
                let order = a.order(b);
                match op {
                    BinaryOperator::Eq => order.is_eq(), BinaryOperator::NotEq => order.is_ne(),
                    BinaryOperator::Gt => order.is_gt(), BinaryOperator::GtEq => order.is_ge(),
                    BinaryOperator::Lt => order.is_lt(), BinaryOperator::LtEq => order.is_le(), _ => false,
                }
            }
        }
    }
}

fn conditions(expr: &Expr, columns: &[Column], binder: &mut Binder, output: &mut Vec<Condition>) -> bool {
    match expr {
        Expr::Nested(expr) => conditions(expr, columns, binder, output),
        Expr::BinaryOp { left, op: BinaryOperator::And, right } =>
            conditions(left, columns, binder, output) && conditions(right, columns, binder, output),
        Expr::BinaryOp { left, op, right } if matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq | BinaryOperator::Gt
                | BinaryOperator::GtEq | BinaryOperator::Lt | BinaryOperator::LtEq) => {
            let Some(left) = Term::compile(left, columns, binder) else { return false };
            let Some(right) = Term::compile(right, columns, binder) else { return false };
            output.push(Condition::Compare(left, op.clone(), right));
            true
        }
        Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            let Some(term) = Term::compile(inner, columns, binder) else { return false };
            output.push(Condition::Null(term, matches!(expr, Expr::IsNull(_))));
            true
        }
        _ => false,
    }
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
        && store.column_defs(&table)?.iter().any(|c|c.name.eq_ignore_ascii_case(&id.value)) { return Ok(None); }
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
