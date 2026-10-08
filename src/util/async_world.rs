//! 一个"单任务"异步运行时：在 await 点之间，由它独占持有整个世界（[`World`]）。
//!
//! Bevy 的资源加载是**跨多帧**完成的，这通常会把所有依赖加载结果的代码挤进"观察者回调"里，
//! 于是每一步顺序约束都退化成"哪个回调碰巧最后触发"。本模块把这套工作改写成一条顺直的
//! `async fn`：
//!
//! ```ignore
//! async fn load(w: AsyncWorld) {
//!     w.spawn(WorldAssetRoot(ground), GroundRoot).await;   // 场景实例就绪时才算完成
//!     w.spawn(WorldAssetRoot(robot), Infantry::default()).await;
//! }
//! ```
//!
//! # 工作原理
//!
//! 任务**从不**在 await 点之间持有 [`World`] 引用。被挂起的任务改为留下一个"作业"（*job*）——
//! 一个装箱的 `FnOnce(&mut World)`——由独占系统 [`drive_async_world`] 代为执行，然后再轮询任务。
//! 作业在**同一帧内**被循环处理，所以连续的 [`AsyncWorld::with_world`] 调用会背靠背地接连执行。
//!
//! 等待是"事件驱动"的，不是轮询。`AsyncWorld::spawn` 会挂上一个实体观察者，观察者触发
//! [`Signal`]；触发时唤醒任务的 [`Waker`]，而唤醒是让 [`drive_async_world`] 再次轮询任务的
//! **唯一**途径。于是从 spawn 到它对应的 `WorldInstanceReady` 之间，任务不消耗任何开销。
//!
//! 新手速记：这不是"多线程并行"，而是"一个可暂停、可恢复的顺序流程"（async/await 的协程模型）。

use bevy::ecs::event::EntityEvent;
use bevy::ecs::system::RunSystemOnce; // 提供 run_system_once_with：跑一次某个系统
use bevy::prelude::*;
use bevy::world_serialization::{WorldAssetRoot, WorldInstanceReady}; // 场景根组件 / "实例已就绪"事件
use std::any::Any; // 类型擦除：把任意具体类型的返回值装进 Box<dyn Any>
use std::future::{Future, poll_fn}; // poll_fn：用一个闭包现场拼出一个 Future
use std::pin::Pin; // Pin：保证被 await 的对象地址不移动（自引用 Future 的前提）
use std::sync::atomic::{AtomicBool, Ordering}; // 原子布尔 + 内存序
use std::sync::{Arc, Mutex}; // 原子引用计数共享 + 互斥锁
use std::task::{Context, Poll, Wake, Waker}; // 异步任务的四大件

/// Work a suspended task wants performed on the world before it is polled again.
/// 被挂起的任务希望"在下次轮询前"对世界做的事。
/// - `Box<dyn FnOnce(&mut World) -> Box<dyn Any + Send> + Send>`：装箱的 trait 对象（类型已擦除），
///   调用一次即可，接收 `&mut World`，返回任意可跨线程的装箱值；
/// - `Any` 让"返回类型"在运行时才确定（配合下面的 `downcast` 再还原成具体类型）。
type WorldJob = Box<dyn FnOnce(&mut World) -> Box<dyn Any + Send> + Send>;

/// Setting this flag is the only thing waking the task does; the driver checks it before polling.
/// 唤醒只做一件事：把标志位置真。驱动器在轮询前检查该标志。
struct TaskWaker {
    woken: AtomicBool,
}

// `impl Wake for TaskWaker`：实现标准库的 Wake trait，让 TaskWaker 能被包成 `Waker`。
impl Wake for TaskWaker {
    // `wake(self: Arc<Self>)`：Wake 要求的形式（接收自身的 Arc），转调下面按引用版本。
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    // `self: &Arc<Self>` 是"任意 self 类型"语法：方法可指定接收者为 Arc<Self>。
    // `store(.., Release)`：释放序写入——保证此前对该任务的写入对读到该标志的线程可见。
    fn wake_by_ref(self: &Arc<Self>) {
        self.woken.store(true, Ordering::Release);
    }
}

/// A notification that completes after a fixed number of arrivals, awaited by the task.
/// 一个"到齐固定次数后才完成"的通知，由任务 await 等待。
///
/// Arriving before anyone awaits is fine — the wait then completes immediately — so there is no
/// race between an observer running and the task reaching its await point.
/// 提前到达也没问题——之后再 await 会立即完成——所以观察者运行与任务到达 await 点之间
/// 不存在竞态。
pub struct Signal {
    // `Mutex` 只为了满足 `Resource` 的 `Sync` 约束（跨线程共享）；驱动器全程独占访问。
    state: Mutex<SignalState>,
}

enum SignalState {
    // 尚未到齐：记录还差几次（`remaining`），以及等待中的 waker（有了就存起来，稍后唤醒）。
    Pending {
        remaining: usize,
        waker: Option<Waker>,
    },
    // 已到齐：完成态。
    Fired,
}

impl Signal {
    /// A signal that completes after `count` arrivals. `0` is already complete.
    /// 一个需要 `count` 次到达才完成的信号。`0` 表示"已完成"。
    pub fn new(count: usize) -> Arc<Self> {
        // 用 `Arc` 包住：信号要在任务与观察者之间共享所有权（两边都能持有）。
        Arc::new(Self {
            state: Mutex::new(match count {
                0 => SignalState::Fired,
                remaining => SignalState::Pending {
                    remaining,
                    waker: None,
                },
            }),
        })
    }

    /// Records one arrival, waking the task on the last one. Extra arrivals are ignored.
    /// 记录一次到达；最后一次到达时唤醒任务。多余的到达会被忽略。
    pub fn arrive(&self) {
        // `lock().unwrap()`：取互斥锁；返回 MutexGuard，作用域结束自动解锁。unwrap 处理中毒。
        let mut state = self.state.lock().unwrap();
        // `&mut *state`：把 MutexGuard 解引用成对内部值的可变借用，以便按变体修改。
        let last = match &mut *state {
            SignalState::Fired => None,
            SignalState::Pending { remaining, waker } => {
                // `saturating_sub`：减到 0 就停，避免下溢。
                *remaining = remaining.saturating_sub(1);
                // `.then(|| ...)`：条件为真才计算并返回 Some——即"刚好减到 0 时"取出 waker。
                (*remaining == 0).then(|| waker.take())
            }
        };
        let Some(waker) = last else {
            return;
        };
        *state = SignalState::Fired;
        // `drop(state)`：**先释放锁**再唤醒。若持有锁时唤醒、而被唤醒方又要拿同一把锁，会死锁。
        drop(state);
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Resolves once every arrival has been recorded.
    /// 等所有到达都记录完毕后完成。
    pub async fn wait(self: Arc<Self>) {
        // `poll_fn(闭包)`：把一个返回 `Poll` 的闭包包装成 Future；闭包每次被轮询时执行。
        // 返回 `Poll::Ready(())` 表示完成；`Poll::Pending` 表示"先挂起，稍后唤醒我"。
        poll_fn(move |cx| {
            let mut state = self.state.lock().unwrap();
            match &mut *state {
                // 已完成：立刻 ready。
                SignalState::Fired => Poll::Ready(()),
                SignalState::Pending { waker, .. } => {
                    // 记下当前任务的 waker，等 `arrive` 时唤醒它。
                    *waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await
    }
}

/// The rendezvous between a suspended task and [`drive_async_world`].
/// 挂起任务与 [`drive_async_world`] 之间的"交接点"。
///
/// At most one job and one result are outstanding, because a task is a single chain of awaits and
/// can only be blocked on one thing.
/// 最多只有一个作业与一个结果在途，因为任务是一条单链的等待序列，同一时刻只可能卡在一件事上。
#[derive(Default)]
struct TaskChannel {
    job: Mutex<Option<WorldJob>>,
    // 作业的返回值，类型已擦除；由任务侧 `downcast` 还原。
    result: Mutex<Option<Box<dyn Any + Send>>>,
}

/// Handle passed to an async task, granting deferred access to the world.
/// 交给异步任务的句柄：用它来"延迟地"访问世界。
#[derive(Clone)]
pub struct AsyncWorld {
    // `Arc` 让任务持有的句柄能穿越 await 点/线程；`#[derive(Clone)]` 使句柄可复制多份。
    channel: Arc<TaskChannel>,
}

impl AsyncWorld {
    /// Spawns a world asset root and resolves once its instance has finished spawning.
    /// 生成一个世界资源根，并在其实例生成完毕后完成等待。
    ///
    /// Taking the [`WorldAssetRoot`] separately from the rest of the bundle is what makes the wait
    /// safe: an entity that never loads a world would never become ready, and this signature makes
    /// that unrepresentable.
    /// 把 [`WorldAssetRoot`] 单独于其余组件之外传参，是让等待"可证明安全"的关键：
    /// 一个从不加载世界的实体永远不会就绪，而这个签名让"忘记传根组件"变得不可能表达。
    pub async fn spawn(&self, root: WorldAssetRoot, extra: impl Bundle) -> Entity {
        self.spawn_observing::<WorldInstanceReady>((root, extra))
            .await
    }

    /// Spawns `bundle` and resolves when `E` is first triggered on the new entity.
    /// 生成 `bundle`，并在新实体上首次触发事件 `E` 时完成等待。
    ///
    /// The observer is attached in the same world job as the spawn, so the event cannot be missed.
    /// 观察者与生成放在**同一个世界作业**里，因此事件不可能被错过。
    pub async fn spawn_observing<E: EntityEvent>(&self, bundle: impl Bundle) -> Entity {
        let ready = Signal::new(1);
        // `clone` 出第二份信号交给观察者持有（信号本身是 Arc，clone 只增加计数）。
        let arrival = ready.clone();

        let entity = self
            .with_world(move |world| {
                let entity = world.spawn(bundle).id();
                // 在同一作业里挂上一次性观察者，避免"生成与监听之间存在空隙"。
                observe_once::<E>(world, entity, arrival);
                entity
            })
            .await;

        // 等观察者报告事件已触发（若已触发则立即返回）。
        ready.wait().await;
        entity
    }

    /// Resolves once `E` has been triggered on every entity `select` returns.
    /// 当 `select` 返回的**每个**实体都触发了事件 `E` 后完成。
    ///
    /// Selection and observation share one world job, so no entity can fire its event in between.
    /// Selecting nothing resolves immediately.
    /// 选取与监听共用同一个世界作业，所以没有实体能在两者之间抢先触发事件。
    /// 什么都没选到时立即完成。
    pub async fn observe_all<E, S>(&self, select: S)
    where
        E: EntityEvent,
        S: FnOnce(&mut World) -> Vec<Entity> + Send + 'static,
    {
        let done = self
            .with_world(move |world| {
                let entities = select(world);
                // 信号需要"实体数量"次到达才完成。
                let signal = Signal::new(entities.len());
                for entity in entities {
                    // 每个实体挂一个一次性观察者，共用同一个信号（clone 只加引用计数）。
                    observe_once::<E>(world, entity, signal.clone());
                }
                signal
            })
            .await;

        done.wait().await;
    }

    /// Runs a one-shot system with `input` and resolves to its output.
    /// 用 `input` 跑一个一次性系统，并完成为它的输出。
    ///
    /// Lets setup that was written as an observer stay a normal Bevy system — queries, `Commands`
    /// and all — while being called at an explicit point in the sequence instead of whenever an
    /// event happens to fire. Deferred commands are applied before this resolves.
    /// 让"本来写成观察者的初始化逻辑"能保持成普通 Bevy 系统——查询、`Commands` 一应俱全——
    /// 却在序列中一个明确的点被调用，而不是"等某个事件碰巧触发"。完成前会先应用延迟命令。
    pub async fn run<I, O, M, S>(&self, system: S, input: I) -> O
    where
        S: IntoSystem<In<I>, O, M> + Send + 'static,
        I: Send + 'static,
        O: Send + 'static,
        M: 'static,
    {
        self.with_world(move |world| {
            world
                .run_system_once_with(system, input)
                .expect("async world one-shot system failed")
        })
        .await
    }

    /// Runs `job` on the world and resolves to its return value.
    /// 在世界中运行 `job`，并完成为它的返回值。
    ///
    /// Resolves within the same frame: the driver serves the job and immediately polls again.
    /// 结果**在同一帧内**就绪：驱动器执行该作业后会立刻再轮询一次。
    pub async fn with_world<T, F>(&self, job: F) -> T
    where
        F: FnOnce(&mut World) -> T + Send + 'static,
        T: Send + 'static,
    {
        let channel = self.channel.clone();
        // `Option` 包裹：闭包是 FnOnce，只能调用一次；用 `take()` 取出后置 None 以防二次调用。
        let mut job = Some(job);

        poll_fn(move |cx| {
            // 先看有没有结果：有就取出、还原具体类型、返回 Ready。
            if let Some(result) = channel.result.lock().unwrap().take() {
                // `downcast::<T>()`：把 `Box<dyn Any>` 还原回具体类型 `Box<T>`（类型不匹配会 Err）。
                let result = result
                    .downcast::<T>()
                    .expect("async world job result type mismatch");
                return Poll::Ready(*result);
            }

            // 首次轮询：把作业放进通道，交给驱动器执行，然后返回 Pending。
            let job = job.take().expect("async world job polled after completion");
            *channel.job.lock().unwrap() = Some(Box::new(move |world| {
                // 把作业的返回值装进 `Box<dyn Any>` 擦除类型，驱动器原样搬运，稍后由上面 downcast 还原。
                Box::new(job(world)) as Box<dyn Any + Send>
            }));
            // 主动唤醒自己：让驱动器知道"有活干"，从而在同一帧继续循环执行该作业。
            cx.waker().wake_by_ref();
            Poll::Pending
        })
        .await
    }
}

/// Records a single arrival on `signal` the first time `E` fires on `entity`.
/// 记录 `signal` 的一次到达——仅在 `E` 首次在 `entity` 上触发时。
///
/// Events like asset reloads can fire more than once; the latch keeps a repeat from consuming
/// another entity's arrival.
/// 像资源重载这类事件可能触发多次；这个"latch"（一次性闩锁）防止重复触发多消耗掉别的实体的配额。
fn observe_once<E: EntityEvent>(world: &mut World, entity: Entity, signal: Arc<Signal>) {
    // 每个实体一个原子闩锁：只有第一次触发会真正 `arrive`。
    let arrived = AtomicBool::new(false);
    world.entity_mut(entity).observe(move |_: On<E>| {
        // `swap(true, ..)` 返回旧值：旧值为 false 说明是首次 → 计数一次；否则忽略。
        if !arrived.swap(true, Ordering::Relaxed) {
            signal.arrive();
        }
    });
}

/// A running async task, stored as a resource for [`drive_async_world`] to poll.
/// 一个正在运行的异步任务，作为资源保存，供 [`drive_async_world`] 轮询。
#[derive(Resource)]
pub struct AsyncWorldTask {
    // `Mutex` only to satisfy `Resource`'s `Sync` bound; the driver always has exclusive access.
    // `Mutex` 只为满足 `Resource` 的 `Sync` 约束；驱动器始终独占访问，不存在真实争用。
    // `Pin<Box<dyn Future<...>>>`：把 Future 装箱并钉住地址——因为 await 生成的 Future 可能是
    // "自引用"的，一旦移动地址就悬空，`Pin` 从类型层面禁止移动。
    future: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,
    waker: Arc<TaskWaker>,
    channel: Arc<TaskChannel>,
}

impl AsyncWorldTask {
    /// Builds a task from a function that receives the world handle.
    /// 用一个"接收世界句柄的函数"构造任务。
    pub fn new<F, Fut>(task: F) -> Self
    where
        F: FnOnce(AsyncWorld) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let channel = Arc::new(TaskChannel::default());
        // `Box::pin(...)`：装箱并钉住；`as Pin<Box<dyn Future...>>` 擦除具体 Future 类型。
        let future = Mutex::new(Box::pin(task(AsyncWorld {
            channel: channel.clone(),
        })) as Pin<Box<dyn Future<Output = ()> + Send>>);

        Self {
            future,
            // Start woken: the task has not been polled even once yet.
            // 起始即"已唤醒"：任务还一次都没被轮询过，必须让驱动器先跑一轮。
            waker: Arc::new(TaskWaker {
                woken: AtomicBool::new(true),
            }),
            channel,
        }
    }
}

/// Polls the task, serving its world jobs, until it blocks on a [`Signal`] or finishes.
/// 轮询任务并代为执行它的世界作业，直到它卡在某个 [`Signal`] 上或运行结束。
///
/// Removes [`AsyncWorldTask`] once the task completes, which also stops this system running.
/// 任务完成后会移除 [`AsyncWorldTask`] 资源，这也让本系统此后不再运行。
pub fn drive_async_world(world: &mut World) {
    // `remove_resource` 把任务从世界取出（独占所有权）；不存在就直接返回。
    let Some(mut task) = world.remove_resource::<AsyncWorldTask>() else {
        return;
    };

    // 用任务的 waker 组装出 Poll 所需的 `Context`。
    let waker = Waker::from(task.waker.clone());
    let mut cx = Context::from_waker(&waker);
    // `get_mut()` 拿到被 Mutex 保护的 Future 的可变引用（这里独占，不会阻塞）。
    let future = task
        .future
        .get_mut()
        .expect("async world task future poisoned");

    // `swap(false)`：读取并清除"被唤醒"标志；只要它被置真就继续循环。
    while task.waker.woken.swap(false, Ordering::AcqRel) {
        // 轮询一次；若任务完成（Ready）则直接返回（此时不把资源放回，等价于销毁任务）。
        if future.as_mut().poll(&mut cx).is_ready() {
            return;
        }

        // Serving a job re-wakes the task, so the loop continues without waiting for a frame.
        // 执行一个作业会再次唤醒任务，所以循环能不用等下一帧就继续。
        let Some(job) = task.channel.job.lock().unwrap().take() else {
            break;
        };
        let result = job(world);
        // 存放结果，供下次轮询时被任务取走。
        *task.channel.result.lock().unwrap() = Some(result);
    }

    // 任务尚未完成：放回资源，等下次唤醒再继续。
    world.insert_resource(task);
}

// ── 测试：验证运行时语义（同帧连续执行、信号计数、提前触发不卡死等）。──
#[cfg(test)]
mod tests {
    use super::*;

    // 测试用资源：记录任务依次执行的步骤名。
    #[derive(Resource, Default, PartialEq, Debug)]
    struct Steps(Vec<&'static str>);

    // 连续的世界作业应在同一帧内依次跑完。
    #[test]
    fn consecutive_world_jobs_run_in_one_frame() {
        let mut world = World::new();
        world.init_resource::<Steps>();
        world.insert_resource(AsyncWorldTask::new(|w: AsyncWorld| async move {
            for step in ["a", "b", "c"] {
                w.with_world(move |world| world.resource_mut::<Steps>().0.push(step))
                    .await;
            }
        }));

        drive_async_world(&mut world);

        assert_eq!(world.resource::<Steps>().0, vec!["a", "b", "c"]);
        assert!(!world.contains_resource::<AsyncWorldTask>());
    }

    // 等待信号的任务应在信号触发时恢复执行。
    #[test]
    fn a_task_awaiting_a_signal_resumes_when_it_fires() {
        #[derive(Resource, Deref)]
        struct Trigger(Arc<Signal>);

        let signal = Signal::new(1);
        let mut world = World::new();
        world.init_resource::<Steps>();
        world.insert_resource(Trigger(signal.clone()));
        world.insert_resource(AsyncWorldTask::new(|w: AsyncWorld| async move {
            let signal = w
                .with_world(|world| world.resource::<Trigger>().0.clone())
                .await;
            w.with_world(|world| world.resource_mut::<Steps>().0.push("before"))
                .await;
            // 卡在这里等待，直到 signal.arrive()。
            signal.wait().await;
            w.with_world(|world| world.resource_mut::<Steps>().0.push("after"))
                .await;
        }));

        drive_async_world(&mut world);
        assert_eq!(world.resource::<Steps>().0, vec!["before"]);

        // Not woken: polling again must not advance the task.
        // 未被唤醒：再次轮询不得推进任务。
        drive_async_world(&mut world);
        assert_eq!(world.resource::<Steps>().0, vec!["before"]);

        signal.arrive();
        drive_async_world(&mut world);
        assert_eq!(world.resource::<Steps>().0, vec!["before", "after"]);
        assert!(!world.contains_resource::<AsyncWorldTask>());
    }

    // 计数信号必须等齐每一次到达才完成。
    #[test]
    fn a_counting_signal_waits_for_every_arrival() {
        #[derive(Resource, Deref)]
        struct Trigger(Arc<Signal>);

        let signal = Signal::new(3);
        let mut world = World::new();
        world.init_resource::<Steps>();
        world.insert_resource(Trigger(signal.clone()));
        world.insert_resource(AsyncWorldTask::new(|w: AsyncWorld| async move {
            let signal = w
                .with_world(|world| world.resource::<Trigger>().0.clone())
                .await;
            signal.wait().await;
            w.with_world(|world| world.resource_mut::<Steps>().0.push("all arrived"))
                .await;
        }));

        drive_async_world(&mut world);
        for _ in 0..2 {
            signal.arrive();
            drive_async_world(&mut world);
            // 还差一次，不应有任何进展。
            assert!(world.resource::<Steps>().0.is_empty());
        }

        signal.arrive();
        drive_async_world(&mut world);
        assert_eq!(world.resource::<Steps>().0, vec!["all arrived"]);

        // Late arrivals must not panic or resurrect the signal.
        // 迟到的到达不得 panic，也不能"复活"信号。
        signal.arrive();
    }

    // count=0 的信号视为已完成。
    #[test]
    fn an_empty_signal_is_already_complete() {
        let mut world = World::new();
        world.init_resource::<Steps>();
        world.insert_resource(AsyncWorldTask::new(|w: AsyncWorld| async move {
            Signal::new(0).wait().await;
            w.with_world(|world| world.resource_mut::<Steps>().0.push("skipped"))
                .await;
        }));

        drive_async_world(&mut world);

        assert_eq!(world.resource::<Steps>().0, vec!["skipped"]);
        assert!(!world.contains_resource::<AsyncWorldTask>());
    }

    // 在 await 之前就已触发的信号不得造成死锁。
    #[test]
    fn a_signal_fired_before_the_wait_does_not_deadlock() {
        let signal = Signal::new(1);
        // 先到达，再交给任务 await。
        signal.arrive();

        let mut world = World::new();
        world.init_resource::<Steps>();
        world.insert_resource(AsyncWorldTask::new(move |w: AsyncWorld| async move {
            signal.wait().await;
            w.with_world(|world| world.resource_mut::<Steps>().0.push("done"))
                .await;
        }));

        drive_async_world(&mut world);

        assert_eq!(world.resource::<Steps>().0, vec!["done"]);
        assert!(!world.contains_resource::<AsyncWorldTask>());
    }
}
