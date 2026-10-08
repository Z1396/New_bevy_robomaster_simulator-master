//! 层级查询工具：按**实体名字**在父子树里找节点。全项目最常用的自制工具之一。
//!
//! 背景问题：Bevy 的实体只有 ID，没有名字查询的原生接口。而本项目加载的车辆/装甲模型是
//! 深层的父子实体树，节点靠 `Name` 组件（如 "GIMBAL"、"ARMOR"、"L_L_RED"）区分。要在树里
//! 精确定位某个节点，需要"从根出发、一层层往下、按名字过滤"的遍历。
//!
//! 本文件提供两样东西：
//! 1. `HierarchyQuery`：自制 `SystemParam`。写进系统函数参数里就能用，一次提供"父查询、
//!    子查询、名字查询"三张表，避免每个系统各自声明一堆 `Query`；
//! 2. `query!` 宏：把"名过滤链"写成一条读起来像路径的表达式，例如
//!    `query!(root_query, .."ARMOR")` 表示"从 root_query 的结果里，取名字以 ARMOR 结尾的子节点"。
//!
//! 使用范式（真实例子见 `robomaster/armor/construct.rs`、`setup.rs`）：
//! ```ignore
//! let root_query = query.of(root).flatten();          // 从 root 起步，固化成可复用的迭代器
//! let armor = query!(root_query, .."ARMOR")?;         // 取恰好一个"名字以 ARMOR 结尾"的节点
//! let iter = query.of(root).any().exact("VEHICLE").flatten(); // 链式写法
//! ```
//!
//! 关键机制：`Hierarchy` 枚举是**惰性**的——每个链式方法只是包一层新的迭代器适配器，
//! 真正遍历发生在最后取值（`one()`）或手动 `into_iter()` 时。`Epilogue` 变体代表"结果为空"，
//! 让后续取值安全地短路（`one()` 返回 None），无需 panic。

// 本项目实现：Either 二选一枚举（下方 IntoIterator 用来统一"有结果/空结果"两种迭代器类型）。
use crate::util::either::Either;
// 导入 SystemParam trait 本身；`#[derive(SystemParam)]` 生成的 impl 需要它在作用域内。
use bevy::ecs::system::SystemParam;
// `Read<T>`：Bevy 0.19 的"无生命周期只读访问"标记，用作 Query 的组件数据参数。
use bevy::ecs::system::lifetimeless::Read;
// Entity / Query / ChildOf / Children / Name 等常用类型都在 prelude。
use bevy::prelude::*;
// 第三方 exact crate 的两个扩展 trait：给迭代器提供 `exact::<N>()`（"恰好 N 个"）与 `into_single()`。
use exact::{ExactExt, ExactOneExt};
// `Empty<Entity>` 类型与 `empty()` 构造函数：零成本、零分配的空迭代器。
use std::iter::{Empty, empty};

#[macro_export]
// 声明式宏：把一串"名过滤 token"翻译成对 `Hierarchy` 的链式方法调用。
// 设计要点：先用公开臂接收输入，再用 `@internal` 私有臂递归消费剩余 token，直到末尾取值。
macro_rules! query {
    // ── 公开入口臂 ──
    // `$query:expr` 捕获"构建器"表达式（通常是个变量），`$($tt:tt)*` 捕获其后全部 token（可选）。
    // 默认 `.clone()` 复制一份再递归——让同一条根查询能被反复使用，不消耗原值。
    ($query: expr, $($tt:tt)*) => {
        $crate::query!(@internal $query.clone(), $($tt)*)
    };
    // `nocopy` 变体：跳过 clone，直接消费传入的构建器（省一次复制，但该值用完即废）。
    (nocopy $query: expr, $($tt:tt)*) => {
        $crate::query!(@internal $query, $($tt)*)
    };
    // ── 内部递归臂（`@internal` 是约定俗成的私有标记，防止外部直接调用）──
    // 从右往左读 token：`.."X"` 是"后缀"——筛"名字以 X 结尾"的子节点。
    (@internal $query: expr, ..$ident:literal $($tt:tt)*) => {
        $crate::query!(@internal $query.suffix($ident) $($tt)*)
    };
    // `"X"..` 是"前缀"——筛"名字以 X 开头"的子节点。
    (@internal $query: expr, $ident:literal.. $($tt:tt)*) => {
        $crate::query!(@internal $query.prefix($ident) $($tt)*)
    };
    // `"X"` 是"精确"——筛"名字恰好等于 X"的子节点。
    (@internal $query: expr, $ident:literal $($tt:tt)*) => {
        $crate::query!(@internal $query.exact($ident) $($tt)*)
    };
    // `...` 是"任意"——不做名字过滤，往下取**全部**子节点（常用于"再钻一层再过滤"）。
    (@internal $query: expr, ... $($tt:tt)*) => {
        $crate::query!(@internal $query.any() $($tt)*)
    };
    // ── 递归终点 ──
    // token 用尽：`one()` 要求"恰好一个"，返回 `Option<Entity>`。
    (@internal $query: expr) => { $query.one() };
    // 末尾只剩一个逗号（如 `query!(q, "X",)`）：同 `one()`。
    (@internal $query: expr, ) => { $query.one() };
    // `ref`：不取单值，而是把构建器本身原样返回，交给调用方手动 `into_iter()` / `.flatten()`。
    (@internal $query: expr, ref) => { $query };
}

/// 自制系统参数：把层级查询需要的三张只读 Query 打包成一个类型。
///
/// **为什么它能当系统参数？** 关键在于下面一行的 `#[derive(SystemParam)]`——该派生宏会自动
/// 生成 `impl SystemParam for HierarchyQuery`，告诉引擎"如何"从世界里为它准备数据。只要这个
/// impl 存在，它就和 `Query`、`Res` 一样可以写在系统函数参数里，由引擎自动注入。
/// （要求：每个字段本身也得是合法的系统参数，这里三个字段都是 `Query`。）
#[derive(SystemParam)]
pub struct HierarchyQuery<'w, 's> {
    // 父关系表：给实体查到它的父实体（`ChildOf` 的 `.0` 即父 Entity）。`parent()` 方法用它。
    pub child_of: Query<'w, 's, Read<ChildOf>>,
    // 子关系表：给实体查到它的子实体列表。`any()`/`suffix()` 等方法用它向下遍历。
    pub children: Query<'w, 's, Read<Children>>,
    // 名字表：`With<ChildOf>` 过滤器限定"只查有父实体的实体"——即**根节点不会被此查询命中**。
    // 这是重要契约：`of(root)` 传入的根只作为遍历起点，不会参与名字过滤。
    pub name: Query<'w, 's, Read<Name>, With<ChildOf>>,
}

// 生命周期说明：`'w` 是"世界数据的借用期"，`'s` 是"系统状态的借用期"——Bevy 用它们保证
// 查询引用的数据在系统执行期间有效。手写时几乎只需原样照抄。
impl<'w, 's> HierarchyQuery<'w, 's> {
    /// 手动构造。系统参数通常由引擎注入，但测试或需要显式传参时会用这个构造函数。
    pub fn new(
        child_of: Query<'w, 's, Read<ChildOf>>,
        children: Query<'w, 's, Read<Children>>,
        name: Query<'w, 's, Read<Name>, With<ChildOf>>,
    ) -> Self {
        Self {
            child_of,
            children,
            name,
        }
    }
}

/// 给"迭代器 + 可克隆"这个组合约束起一个名字。
/// 因为链式方法的返回类型写成 `impl HierarchyIter`，需要一个 trait 来指代这组约束；
/// trait 没有方法，纯粹是类型层面的"标签"（marker trait）。
pub trait HierarchyIter: Iterator<Item = Entity> + Clone {}

// 空白实现（blanket impl）：任何"产出 Entity 且可 Clone 的迭代器"都自动满足 HierarchyIter。
// 有了它，无需为每种迭代器类型逐个 impl。
impl<I: Iterator<Item = Entity> + Clone> HierarchyIter for I {}

impl<'w, 's> HierarchyQuery<'w, 's> {
    /// 查询入口：从 `root` 出发，返回一个可继续链式调用的 `Hierarchy`。
    /// 内部把起点包成"只含 root 一个元素的迭代器"，真正的向下遍历在后续方法里才发生。
    pub fn of<'q>(&'q self, root: Entity) -> Hierarchy<'q, 'w, 's, impl HierarchyIter> {
        Hierarchy::Prologue::<'q, 'w, 's> {
            // `vec![root].into_iter()`：单元素迭代器，作为惰性链的"种子"。
            lazy: vec![root].into_iter(),
            // `param: self` 借用 `HierarchyQuery`，供后续方法查 children/name。
            param: self,
        }
    }
}

/// 惰性查询链的状态机。泛型 `IterType` 是"当前这一环包装的迭代器类型"。
/// - `Prologue { lazy, param }`：还有待展开的中间态——`lazy` 是当前迭代器，`param` 是查询表；
/// - `Epilogue`：终结态，代表"已经没有结果了"。下游取值方法见到它一律安全短路。
/// `where 's: 'q, 'w: 's` 是生命周期约束：`'q`（链自身的借用）短于 `'s`，`'s` 短于 `'w`。
#[derive(Copy, Clone)]
pub enum Hierarchy<'q, 'w, 's, IterType: HierarchyIter>
where
    's: 'q,
    'w: 's,
{
    Prologue {
        lazy: IterType,
        param: &'q HierarchyQuery<'w, 's>,
    },
    Epilogue,
}

/// 生成"按名字过滤子节点"的链式方法。一次定义、五个方法复用，靠参数区分行为：
/// - `$v:vis`：生成方法的可见性（这里是 `pub`）；
/// - `$method_name:ident`：生成的方法名（suffix/prefix/exact/with/without）；
/// - `$method:ident`：真正调用的字符串方法（ends_with/starts_with/eq/contains）；
/// - `$($prefix:tt)*`：可选的前缀 token（`without` 会传一个 `!`，把结果取反）。
macro_rules! impl_hierarchy {
    ($v:vis $method_name:ident,$method:ident $($prefix:tt)*) => {
        // `#[must_use]`：若调用方丢弃返回值，编译器发警告（链式构建器的结果通常都该接着用）。
        // `#[inline]`：建议编译器内联——这类小适配器内联后没有额外开销。
        #[must_use]
        #[inline]
        // 接收 `suffix: T`，`T: Into<&'q str>` 表示 `&str` 和 `String` 都能传。
        $v fn $method_name<T: Into<&'q str>>(
            self,
            suffix: T,
        ) -> Hierarchy<'q, 'w, 's, impl HierarchyIter> {
            match self {
                Hierarchy::Prologue { lazy, param } => {
                    // `#[allow(unused_assignments)]`：当 `$prefix` 为空时,下面 `_suffix` 仍被使用,
                    // 但宏展开的某些形态会触发"赋值未使用"告警,这里统一静默。
                    #[allow(unused_assignments)]
                    let _suffix = suffix.into();
                    // 核心：把当前迭代器的每个实体，替换成"它的所有子节点"（摊平一层），
                    // 再按名字谓词过滤。`.flatten()` 把 `Option<Children 迭代器>` 摊平成子节点序列。
                    let flatten = lazy
                        .filter_map(|current| {
                            param
                                .children
                                .get(current)
                                .ok()
                                .map(|children| children.into_iter())
                        })
                        .flatten()
                        // `.copied()`：`Children` 迭代项是 `&Entity`，复制成 `Entity`。
                        .copied()
                        // `move`：闭包按值捕获 `_suffix`（字符串）与 `param`（引用）。
                        .filter(move |&child| {
                            // 取子节点的名字；查不到名字的节点直接落选。
                            if let Ok(_name) = param.name.get(child) {
                                // `$($prefix)*` 展开为空的"或 `!`"——`without` 走取反分支。
                                // `_name.as_ref()` 取 `&str`，再调用 ends_with/starts_with/eq/contains。
                                $($prefix)* _name.as_ref().$method(_suffix)
                            } else {
                                false
                            }
                        });
                    Hierarchy::Prologue {
                        lazy: flatten,
                        param,
                    }
                }
                // 已经是空结果，原样保持空。
                Hierarchy::Epilogue => Hierarchy::Epilogue,
            }
        }
    };
}

impl<'q, 'w, 's, IterType: HierarchyIter> Hierarchy<'q, 'w, 's, IterType> {
    // 下面是五个链式方法，语义逐条：往下取一层子节点并做名字过滤。
    // `.."X"` ⇔ suffix ⇔ 名字以 X 结尾；`"X"..` ⇔ prefix ⇔ 名字以 X 开头。
    impl_hierarchy!(pub suffix, ends_with);
    impl_hierarchy!(pub prefix, starts_with);
    // `"X"` ⇔ exact ⇔ 名字恰好等于 X。
    impl_hierarchy!(pub exact, eq);
    // with ⇔ 名字包含 X；without ⇔ 名字**不**包含 X（末尾的 `!` 让谓词取反）。
    impl_hierarchy!(pub with, contains);
    impl_hierarchy!(pub without, contains !);

    /// 把惰性链"固化"为具体结果，并顺便短路空结果。两个作用：
    /// 1. 交错包装的迭代器类型会层层嵌套、越滚越复杂；`collect::<Vec<_>>()` 把它压成
    ///    单一的 `Vec::IntoIter`，避免类型爆炸，也让结果可多轮复用（`Clone`）；
    /// 2. 若结果为空，直接转成 `Epilogue`，后续 `one()` 立刻返回 None，无需空跑。
    #[must_use]
    #[inline]
    pub fn flatten(self) -> Hierarchy<'q, 'w, 's, impl HierarchyIter> {
        match self {
            Hierarchy::Prologue { lazy, param } => {
                let vec = lazy.collect::<Vec<_>>();
                if vec.is_empty() {
                    return Hierarchy::Epilogue;
                }
                Hierarchy::Prologue {
                    lazy: vec.into_iter(),
                    param,
                }
            }
            Hierarchy::Epilogue => Hierarchy::Epilogue,
        }
    }

    /// 往上走一层：把当前每个实体替换成它的父实体（没有父的实体被丢弃）。
    /// 契约：`Epilogue`（空结果）上调用会 panic——所以应保证链上先有结果再 `parent()`。
    #[must_use]
    #[inline]
    pub fn parent(self) -> Hierarchy<'q, 'w, 's, impl HierarchyIter> {
        match self {
            Hierarchy::Prologue { lazy, param } => {
                // `filter_map`：查不到父实体就丢弃；查到则取出 `ChildOf` 的 `.0`（父 Entity）。
                let flatten = lazy.filter_map(|current| {
                    param.child_of.get(current).ok().map(|children| children.0)
                });
                Hierarchy::Prologue {
                    lazy: flatten,
                    param,
                }
            }
            Hierarchy::Epilogue => {
                // 空结果上无法取父：这是编程错误，直接 panic 暴露出来。
                panic!("parent on epilogue");
            }
        }
    }

    /// 往下走一层并取**全部**子节点（不做名字过滤），等价于 `...` 臂背后的 `any()`。
    /// 契约同 `parent()`：不能在 `Epilogue` 上调用。
    #[must_use]
    #[inline]
    pub fn any(self) -> Hierarchy<'q, 'w, 's, impl HierarchyIter> {
        match self {
            Hierarchy::Prologue { lazy, param } => {
                let flatten = lazy
                    // 每个实体 → 它的 Children 迭代器 → 摊平成一串子节点。
                    .filter_map(|current| {
                        param
                            .children
                            .get(current)
                            .ok()
                            .map(|children| children.into_iter())
                    })
                    .flatten()
                    .copied();
                Hierarchy::Prologue {
                    lazy: flatten,
                    param,
                }
            }
            Hierarchy::Epilogue => {
                // 空结果上无法再往下：编程错误，panic。
                panic!("any on epilogue");
            }
        }
    }

    /// 取"**恰好一个**"实体并结束链，返回 `Option<Entity>`。
    /// - `lazy.exact::<1>()`：来自 exact crate，要求迭代器恰好产出 1 个元素，否则返回 Err；
    /// - `.ok()`：把 `Result` 转成 `Option`（Err 丢弃）；
    /// - `.into_single()`：从"恰好一个"的结果里取出那个唯一的实体。
    /// 因此 0 个或多个都会得到 `None`；`Epilogue` 也直接返回 `None`。
    #[must_use]
    #[inline]
    pub fn one(self) -> Option<Entity> {
        match self {
            Hierarchy::Prologue { lazy, .. } => lazy.exact::<1>().ok().into_single(),
            Hierarchy::Epilogue => None,
        }
    }
}

// 让 `Hierarchy` 能直接用于 `for x in h { ... }` 或 `.into_iter()`。
// 关键点：返回类型是 `Either<Empty<Entity>, IterType>`——
// - `Prologue` 装真实迭代器（`Either::Right(lazy)`）；
// - `Epilogue` 装空迭代器（`Either::Left(empty())`）。
// `std::iter::empty()` 是**零成本、零分配**的空迭代器，所以"没找到"这个分支不产生任何堆开销，
// 这就是文件头所说的"零成本空迭代"。
impl<'q, 'w, 's, IterType: HierarchyIter> IntoIterator for Hierarchy<'q, 'w, 's, IterType>
where
    IterType: Iterator<Item = Entity>,
{
    type Item = Entity;
    type IntoIter = Either<Empty<Entity>, IterType>;

    fn into_iter(self) -> Self::IntoIter {
        match self {
            Hierarchy::Prologue { lazy, .. } => Either::Right(lazy),
            Hierarchy::Epilogue => Either::Left(empty()),
        }
    }
}
