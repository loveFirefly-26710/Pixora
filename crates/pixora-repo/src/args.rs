//! 查询参数的动态构造。
//!
//! 过滤条件是按需拼的，绑定数量在编译期定不下来，所以不能只用链式 `.bind()`。
//! 这里把值先攒在自己的列表里（可克隆、可复用），到执行前再转成 sqlx 的参数集合，
//! 这样「总数查询」与「列表查询」能共用同一组条件。
//!
//! 只用六种可移植的值类型：整数、文本、布尔，以及它们的可空形式。
//! 时间戳与 JSON 都以文本入库，用 `ts()` / `json_text()` 转好再传进来。

#[derive(Default, Clone)]
pub struct Args {
    values: Vec<OwnedValue>,
}

#[derive(Clone)]
enum OwnedValue {
    Int(i64),
    OptInt(Option<i64>),
    Text(String),
    OptText(Option<String>),
}

impl Args {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(mut self, value: impl Into<String>) -> Self {
        self.values.push(OwnedValue::Text(value.into()));
        self
    }

    pub fn opt_text(mut self, value: Option<String>) -> Self {
        self.values.push(OwnedValue::OptText(value));
        self
    }

    pub fn int(mut self, value: i64) -> Self {
        self.values.push(OwnedValue::Int(value));
        self
    }

    pub fn opt_int(mut self, value: Option<i64>) -> Self {
        self.values.push(OwnedValue::OptInt(value));
        self
    }

    /// 布尔值以 0/1 整数入库。
    ///
    /// `sqlx::Any` 在 SQLite 上把布尔列识别成整数，`bool` 解码会直接报类型不匹配；
    /// 三种方言统一存整数后，读写都是一条路径，也不会踩到各家的布尔映射差异。
    pub fn bool(mut self, value: bool) -> Self {
        self.values.push(OwnedValue::Int(if value { 1 } else { 0 }));
        self
    }

    /// `Option<T>` 里是文本时省一次 `map`。
    pub fn opt_text_owned<T: Into<String>>(self, value: Option<T>) -> Self {
        self.opt_text(value.map(Into::into))
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// 转成 sqlx 的参数集合。类型支持在编译期就已确定，这里不会失败。
    pub fn to_arguments(&self) -> sqlx::any::AnyArguments<'static> {
        use sqlx::Arguments;

        let mut out = sqlx::any::AnyArguments::default();
        for value in &self.values {
            let added = match value {
                OwnedValue::Int(inner) => out.add(*inner),
                OwnedValue::OptInt(Some(inner)) => out.add(*inner),
                OwnedValue::OptInt(None) => out.add(None::<i64>),
                OwnedValue::Text(inner) => out.add(inner.clone()),
                OwnedValue::OptText(Some(inner)) => out.add(inner.clone()),
                OwnedValue::OptText(None) => out.add(None::<String>),
            };
            debug_assert!(added.is_ok(), "只使用受支持的值类型");
        }
        out
    }

    pub fn take(self) -> sqlx::any::AnyArguments<'static> {
        self.to_arguments()
    }
}

/// 条件拼装器：把「AND 还是 WHERE」和「参数按顺序加入」两件事收在一起。
///
/// 拼进 SQL 的只有调用方写死的片段，值一律走参数绑定。
#[derive(Default)]
pub struct WhereBuilder {
    sql: String,
    args: Args,
    first: bool,
}

impl WhereBuilder {
    pub fn new() -> Self {
        Self {
            sql: String::new(),
            args: Args::new(),
            first: true,
        }
    }

    /// 追加一个条件。`cond` 里用 `?` 占位，值在 `values` 里按顺序给出。
    pub fn and<F>(mut self, cond: &str, values: F) -> Self
    where
        F: FnOnce(Args) -> Args,
    {
        if self.first {
            self.sql.push_str(" WHERE ");
            self.first = false;
        } else {
            self.sql.push_str(" AND ");
        }
        self.sql.push_str(cond);
        self.args = values(self.args);
        self
    }

    /// 无条件时返回空串，可直接接在表名后面。
    pub fn sql(&self) -> &str {
        &self.sql
    }

    pub fn sql_string(&self) -> String {
        self.sql.clone()
    }

    pub fn args(self) -> Args {
        self.args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn where_builder_stitches_conditions() {
        let builder = WhereBuilder::new()
            .and("a = ?", |args| args.int(1))
            .and("b LIKE ?", |args| args.text("%x%"));
        assert_eq!(builder.sql(), " WHERE a = ? AND b LIKE ?");
        assert_eq!(builder.args().len(), 2);
    }

    #[test]
    fn empty_builder_produces_nothing() {
        assert_eq!(WhereBuilder::new().sql(), "");
        assert!(WhereBuilder::new().args().is_empty());
    }

    #[test]
    fn bool_binds_as_integer() {
        let args = Args::new().bool(true).bool(false);
        assert_eq!(args.len(), 2);
    }

    #[test]
    fn args_are_reusable() {
        let args = Args::new().text("x").int(7).bool(true);
        assert_eq!(args.len(), 3);
        let cloned = args.clone();
        assert_eq!(cloned.len(), 3);
    }
}
