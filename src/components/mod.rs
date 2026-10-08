//! 组件定义总入口：把 camera / infantry / physics 三个子模块聚合成一类，
//! 并通过 `pub use *` 重导出——于是别处写 `crate::components::Controlled` 就能直接用，
//! 不必关心它实际定义在哪个子文件里。（`pub use x::*` 会把 x 的全部公开项搬到本模块的名字空间。）

mod camera;
mod infantry;
mod physics;

pub use camera::*;
pub use infantry::*;
pub use physics::*;
