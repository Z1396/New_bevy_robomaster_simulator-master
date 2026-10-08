//! `Either<L, R>`：一个"二选一"的枚举，并手写它的 `Iterator` 实现。
//!
//! 用途：有时一段代码要返回"两种迭代器中的哪一种"，但两边的具体类型不同，无法直接写
//! 同一个返回类型。`Either` 把两者包成同一个类型，谁在跑就由谁实现 `next()`。
//! 关键优点：这是**零成本**的——`Left`/`Right` 只是把值放进同一个枚举的哪个字段，
//! 不额外堆分配、不做动态派发（对比 `Box<dyn Iterator>` 要堆分配 + 虚调用）。
//!
//! 本项目里它专给 `util/entity_query.rs` 的 `IntoIterator` 用：层级查询结果要么是"真实迭代器"
//! （`Right`），要么是"空迭代器"（`Left`，即 `std::iter::empty()`），两者统一成一个类型。

// `enum` = 枚举：任一时刻**只有一个**变体存活，天然表达"非此即彼"。
// 泛型 `<L, R>` 让两个变体可以承载完全不同的类型。
pub enum Either<L, R> {
    Left(L),
    Right(R),
}

// 为 `Either` 实现标准库的 `Iterator` trait（行为接口）。
// 这里多引入一个泛型 `T` 表示"元素类型"，并用 `where` 约束：左右两边必须产出**同一种** `Item = T`，
// 否则没法把它们当作同一个迭代器用。
impl<L, R, T> Iterator for Either<L, R>
where
    L: Iterator<Item = T>,
    R: Iterator<Item = T>,
{
    // 关联类型：本迭代器产出的元素类型。
    type Item = T;

    // 推进一步：`match self` 把控制权交给当前存活的那个变体，由它决定"还有没有下一个"。
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Either::Left(l) => l.next(),
            Either::Right(r) => r.next(),
        }
    }

    // 大小提示：把上层的 `size_hint` 原样透传。必须实现它，否则 `collect()`、`Vec::with_capacity`
    // 等下游适配器无法预分配容量，性能会退化。
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Either::Left(l) => l.size_hint(),
            Either::Right(r) => r.size_hint(),
        }
    }
}
