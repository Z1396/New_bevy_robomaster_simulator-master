//! 配置：把 `config.toml` 里的参数解析成一组类型安全的结构体，并支持**热重载**。
//!
//! 三层结构：
//! 1. `SimulationConfig` 是根结构（同时是 Bevy 资源），其字段各是一"块"子配置；
//! 2. 每个子配置结构体都带默认值（手写 `impl Default`），TOML 里没写的字段自动取默认；
//! 3. `ConfigPlugin` 在启动时加载配置、启动文件监听；之后每次 `config.toml` 被保存，
//!    `config_hot_reload` 系统都会重新加载并就地更新资源（部分字段还会同步到物理资源）。
//!
//! 热重载数据流：`notify`（后台线程）侦测文件改动 → 经 crossbeam channel 发送事件 →
//! 主线程系统 `config_hot_reload` 用 `try_recv` 非阻塞取事件 → 重新解析 TOML → 覆盖资源。
//!
//! 单位约定（各字段注释已标注）：角度类配置多以"度(deg)"存储、使用处再 `to_radians()` 转弧度；
//! 角速度 rad/s；线速度 m/s；距离 m；频率 Hz；质量 kg。改配置时务必注意单位。

use avian3d::prelude::SubstepCount;
use bevy::prelude::*;
// crossbeam 的收发端与无界通道构造函数；用于"文件监听线程 → 主线程"的线程安全通信。
use crossbeam_channel::{Receiver, Sender, unbounded};
// notify 库：跨平台文件系统监听（Event=事件，RecommendedWatcher=平台推荐监听器，Watcher=trait）。
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
// serde 的反序列化派生：从 TOML 文本构造结构体。
use serde::Deserialize;
use std::path::Path;

// `#[derive(Resource)]` 使其成为全局唯一资源；`Deserialize` 从 TOML 解析；
// `Reflect` 生成运行时类型信息（egui 检视器/调试用）；`Clone` 便于整份替换（热重载时）。
#[derive(Resource, Deserialize, Reflect, Clone)]
#[reflect(Resource)]
pub struct SimulationConfig {
    // `#[serde(default)]`：TOML 里缺这个字段就用该类型的 `Default::default()` 补上。
    #[serde(default)]
    pub window: WindowConfig,
    #[serde(default)]
    pub debug: DebugConfig,
    #[serde(default)]
    pub preview: PreviewConfig,
    #[serde(default)]
    pub render: RenderConfig,
    #[serde(default)]
    pub capture: CapturePipelineConfig,
    #[serde(default)]
    pub livox_ros: LivoxRosConfig,
    // 以下四项**没有** `#[serde(default)]`：config.toml 必须提供对应小节，否则整体解析失败
    //（失败时 Default::default() 会回退到全默认值，见文件末尾）。
    pub physics: PhysicsConfig,
    pub vehicle: VehicleConfig,
    #[serde(default)]
    pub mecanum: MecanumConfig,
    pub projectile: ProjectileConfig,
    pub camera: CameraConfig,
}

// 窗口子配置。`Deserialize` 从 TOML 读；`Reflect`+`Clone` 便于反射/复制。
#[derive(Deserialize, Reflect, Clone)]
pub struct WindowConfig {
    // 垂直同步/呈现模式的字符串（由 main.rs 的 present_mode_from_config 解析成枚举）。
    pub present_mode: String,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            // Uncap rendering by default so off-screen capture (Talos/ROS2) can exceed 60Hz.
            // 默认不锁帧：让离屏采集（Talos/ROS2）能超过 60Hz 运行。
            present_mode: "auto_no_vsync".to_string(),
        }
    }
}

#[derive(Deserialize, Reflect, Clone)]
// 结构体级 `#[serde(default)]`：任何缺失字段都用各自类型的默认值补齐。
#[serde(default)]
pub struct DebugConfig {
    // 是否启用 egui 调试界面（总开关）。
    pub egui: bool,
    // 是否启用运行时实体/组件检视器（需 egui 开启）。
    pub inspector: bool,
    // 是否打印 FPS/帧时间诊断日志。
    pub diagnostics: bool,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            egui: false,
            inspector: false,
            diagnostics: false,
        }
    }
}

#[derive(Deserialize, Reflect, Clone)]
pub struct PreviewConfig {
    // 是否启用预览（离屏渲染预览通道）。
    pub enabled: bool,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Deserialize, Reflect, Clone)]
#[serde(default)]
pub struct RenderConfig {
    // 主平行光的照度，单位：勒克斯（lux）。
    pub illuminance: f32,
    // 是否开启阴影（阴影贴图开销大）。
    pub shadows: bool,
    // 主相机是否开启 FXAA 抗锯齿。
    pub main_camera_fxaa: bool,
    // `#[serde(alias = "...")]`：接受旧键名，兼容历史配置文件。
    #[serde(alias = "main_camera_metalfx_temporal")]
    pub metalfx_temporal: bool,
    #[serde(alias = "main_camera_metalfx_frame_generation")]
    pub metalfx_frame_generation: bool,
    #[serde(alias = "main_camera_metalfx_scale")]
    pub metalfx_scale: f32,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            illuminance: 50.0,
            shadows: false,
            main_camera_fxaa: false,
            // `cfg!(...)`：编译期判断目标系统，macOS 上默认开启 MetalFX 时域升采样。
            metalfx_temporal: cfg!(target_os = "macos"),
            metalfx_frame_generation: false,
            metalfx_scale: 2.0,
        }
    }
}

#[derive(Deserialize, Reflect, Clone)]
#[serde(default)]
pub struct PhysicsConfig {
    // 每个物理步内部细分的子步数：越大越稳定、越费 CPU。
    pub substep_count: u32,
    // 固定物理频率，单位：Hz（与渲染帧率解耦，决定 FixedUpdate 节拍）。
    pub fixed_hz: f64,
}

impl Default for PhysicsConfig {
    fn default() -> Self {
        Self {
            substep_count: 8,
            fixed_hz: 120.0,
        }
    }
}

#[derive(Deserialize, Reflect, Clone)]
#[serde(default)]
pub struct VehicleConfig {
    // 底盘偏航(yaw)角速度，单位：rad/s。
    pub rotation_speed: f32,
    // 偏航角加速度上限，单位：rad/s²（限制角速度变化率，带来"加速感"）。
    pub yaw_acceleration: f32,
    // 底盘俯仰/侧倾角速度，单位：rad/s。
    pub tilt_rotation_speed: f32,
    // 云台旋转角速度，单位：rad/s。
    pub gimbal_rotation_speed: f32,
    // 云台俯仰角限位（正负对称），单位：rad（0.785 ≈ 45°）。
    pub gimbal_pitch_limit: f32,
    // 底盘最大线速度，单位：m/s。
    pub max_speed: f32,
    // 底盘线加速度，单位：m/s²。
    pub linear_acceleration: f32,
    // 加速曲线指数（无量纲，>1 时低速更灵敏/高速更"绵"）。
    pub acceleration_exponent: f32,
    // 嵌套的云台 PID 配置；`#[serde(default)]` 允许 TOML 省略该小节。
    #[serde(default)]
    pub gimbal_pid: GimbalPidConfig,
}

impl Default for VehicleConfig {
    fn default() -> Self {
        Self {
            rotation_speed: 3.0,
            yaw_acceleration: 24.0,
            tilt_rotation_speed: 3.0,
            gimbal_rotation_speed: 3.0,
            gimbal_pitch_limit: 0.785,
            max_speed: 4.0,
            linear_acceleration: 8.0,
            acceleration_exponent: 10.0,
            gimbal_pid: GimbalPidConfig::default(),
        }
    }
}

/// Gains for the closed-loop gimbal tracking used by every remote/auto-aim command.
/// The two axes carry different inertia and travel limits, so they are tuned apart.
/// 云台闭环追踪的增益参数——所有远程/自瞄指令都走这套 PID。
/// 偏航与俯仰两轴的惯量、行程不同，因此分别整定（两条独立参数）。
#[derive(Deserialize, Reflect, Clone)]
#[serde(default)]
pub struct GimbalPidConfig {
    // 偏航轴 PID。
    pub yaw: GimbalAxisPidConfig,
    // 俯仰轴 PID。
    pub pitch: GimbalAxisPidConfig,
}

impl Default for GimbalPidConfig {
    fn default() -> Self {
        Self {
            yaw: GimbalAxisPidConfig {
                kp: 12.0,
                ki: 0.5,
                kd: 0.35,
                integral_limit: 0.5,
                max_rate: 20.0,
            },
            pitch: GimbalAxisPidConfig {
                kp: 10.0,
                ki: 0.5,
                kd: 0.3,
                integral_limit: 0.5,
                max_rate: 12.0,
            },
        }
    }
}

#[derive(Deserialize, Reflect, Clone)]
#[serde(default)]
pub struct GimbalAxisPidConfig {
    // 比例增益（无量纲）。
    pub kp: f32,
    // 积分增益（无量纲）。
    pub ki: f32,
    // 微分增益（无量纲）。
    pub kd: f32,
    /// Bound on the accumulated error term (rad·s), guards against windup.
    /// 积分项累积误差的限幅，单位：rad·s（防止积分饱和 windup）。
    pub integral_limit: f32,
    /// Saturation of the commanded angular rate for this axis (rad/s).
    /// 该轴指令角速度的饱和上限，单位：rad/s。
    pub max_rate: f32,
}

impl Default for GimbalAxisPidConfig {
    fn default() -> Self {
        Self {
            kp: 12.0,
            ki: 0.5,
            kd: 0.35,
            integral_limit: 0.5,
            max_rate: 20.0,
        }
    }
}

#[derive(Deserialize, Reflect, Clone)]
#[serde(default)]
pub struct MecanumConfig {
    // 麦克纳姆轮半径，单位：m。
    pub wheel_radius_m: f32,
    // 半轴距（前后轮中心距的一半），单位：m。
    pub half_wheelbase_m: f32,
    // 半轮距（左右轮中心距的一半），单位：m。
    pub half_trackwidth_m: f32,
}

impl Default for MecanumConfig {
    fn default() -> Self {
        Self {
            wheel_radius_m: 0.076,
            half_wheelbase_m: 0.18,
            half_trackwidth_m: 0.15,
        }
    }
}

#[derive(Deserialize, Reflect, Clone)]
pub struct ProjectileConfig {
    // 子弹存活时长，单位：s（超时销毁）。
    pub lifetime: f32,
    // 子弹初速度，单位：m/s。
    pub speed: f32,
    // 射击冷却时间，单位：s。
    pub cooldown: f32,
    // 子弹直径，单位：m（17mm = 0.017）。
    pub diameter: f32,
    // UAV 子弹模型尺寸，单位：m。
    pub uav_size: f32,
    // UAV 子弹速度，单位：m/s。
    pub uav_vel: f32,
    // 子弹质量，单位：kg。
    pub mass: f32,
    // 碰撞摩擦系数（无量纲）。
    pub friction: f32,
    // 线性阻尼，单位：1/s（越大越快减速）。
    pub linear_damping: f32,
    // 嵌套的空气阻力配置；`#[serde(default)]` 允许省略。
    #[serde(default)]
    pub aerodynamics: ProjectileAerodynamicsConfig,
}

#[derive(Deserialize, Reflect, Clone)]
pub struct ProjectileAerodynamicsConfig {
    // 是否启用空气阻力计算。
    pub enabled: bool,
    // 空气密度，单位：kg/m³。
    pub air_density: f32,
    // 阻力系数（无量纲）。
    pub drag_coefficient: f32,
    // 风速矢量，单位：m/s（世界坐标系 [x, y, z]）。
    pub wind: [f32; 3],
}

impl Default for ProjectileAerodynamicsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // kg/m^3 - air density at sea level (15°C)
            // 单位 kg/m³ —— 海平面（15°C）空气密度。
            air_density: 1.225,
            // Drag coefficient for a smooth sphere, typical Re for 17mm @ ~25m/s.
            // 光滑球体的阻力系数（17mm 弹丸在约 25m/s 时的典型雷诺数下取值）。
            drag_coefficient: 0.47,
            // m/s - wind velocity in world coordinates.
            // 单位 m/s —— 世界坐标系下的风速。
            wind: [0.0, 0.0, 0.0],
        }
    }
}

#[derive(Deserialize, Reflect, Clone)]
pub struct CameraConfig {
    // 相机垂直视场角，单位：度(deg)（使用处会 to_radians() 转弧度）。
    pub fov: f32,
    // 自由相机移动速度，单位：m/s。
    pub free_move_speed: f32,
    // 跟随相机相对目标的偏移，单位：m（[x, y, z]）。
    pub follow_offset: [f32; 3],
    // 鼠标灵敏度（像素位移 → 角度的比例系数）。
    pub mouse_sensitivity: f32,
}

#[derive(Deserialize, Reflect, Clone)]
#[serde(default)]
pub struct CapturePipelineConfig {
    // 彩色采集通道配置。
    pub color: CaptureStreamConfig,
    // 深度采集通道配置。
    pub depth: DepthCaptureConfig,
}

impl Default for CapturePipelineConfig {
    fn default() -> Self {
        Self {
            color: CaptureStreamConfig::default(),
            depth: DepthCaptureConfig::default(),
        }
    }
}

#[derive(Deserialize, Reflect, Clone)]
#[serde(default)]
pub struct CaptureStreamConfig {
    // 采集图像宽度，单位：像素。
    pub width: u32,
    // 采集图像高度，单位：像素。
    pub height: u32,
}

impl Default for CaptureStreamConfig {
    fn default() -> Self {
        Self {
            width: 1440,
            height: 1080,
        }
    }
}

#[derive(Deserialize, Reflect, Clone)]
#[serde(default)]
pub struct DepthCaptureConfig {
    // 深度图宽度，单位：像素。
    pub width: u32,
    // 深度图高度，单位：像素。
    pub height: u32,
    // 近裁剪面距离，单位：m。
    pub near: f32,
    // 远裁剪面距离，单位：m。
    pub far: f32,
}

impl Default for DepthCaptureConfig {
    fn default() -> Self {
        Self {
            width: 640,
            height: 480,
            near: 0.1,
            far: 80.0,
        }
    }
}

#[derive(Deserialize, Reflect, Clone)]
#[serde(default)]
pub struct LivoxRosConfig {
    // 是否启用 Livox 激光雷达（ROS 输出）。
    pub enabled: bool,
    // 发布到 ROS 的坐标系名（frame id）。
    pub frame_id: String,
    // 点云发布频率，单位：Hz。
    pub publish_freq: f32,
    // 每秒生成的点数，单位：点/秒。
    pub points_per_second: u32,
    // 扫描线数。
    pub line_num: u8,
    // 默认标签值。
    pub tag_default: u8,
    // 默认强度值。
    pub intensity_default: f32,
}

impl Default for LivoxRosConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            frame_id: "livox_frame".to_string(),
            publish_freq: 10.0,
            points_per_second: 100_000,
            line_num: 6,
            tag_default: 0,
            intensity_default: 100.0,
        }
    }
}

impl SimulationConfig {
    /// 从当前工作目录的 `config.toml` 读取并解析配置。
    /// 返回 `Result`：文件缺失或 TOML 语法错误都以 `Err` 上报（错误被装箱成 trait 对象以便统一返回）。
    pub fn load() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // `read_to_string` 失败（文件不存在等）会经 `?` 直接返回 Err。
        let content = std::fs::read_to_string("config.toml")?;
        // `toml::from_str` 按各结构体的 `#[derive(Deserialize)]` 把文本解析成 `SimulationConfig`。
        Ok(toml::from_str(&content)?)
    }
}

impl Default for SimulationConfig {
    fn default() -> Self {
        // `unwrap_or_else`：加载成功就用它；失败则打一条警告并回退到下面的全默认值。
        Self::load().unwrap_or_else(|e| {
            warn!("Failed to load config.toml: {}, using defaults", e);
            Self {
                window: WindowConfig::default(),
                debug: DebugConfig::default(),
                preview: PreviewConfig::default(),
                render: RenderConfig::default(),
                capture: CapturePipelineConfig::default(),
                livox_ros: LivoxRosConfig::default(),
                physics: PhysicsConfig::default(),
                vehicle: VehicleConfig::default(),
                mecanum: MecanumConfig::default(),
                projectile: ProjectileConfig {
                    lifetime: 5.0,
                    speed: 25.0,
                    cooldown: 0.1,
                    diameter: 0.017,
                    mass: 0.017,
                    friction: 1.1,
                    linear_damping: 0.0,
                    aerodynamics: ProjectileAerodynamicsConfig::default(),
                    uav_size: 1.0,
                    uav_vel: 2.0,
                },
                camera: CameraConfig {
                    fov: 45.0,
                    free_move_speed: 8.0,
                    follow_offset: [0.0, 3.0, 2.0],
                    mouse_sensitivity: 0.003,
                },
            }
        })
    }
}

// 保存文件监听器与事件接收端。作为资源常驻，其存在本身就维持着监听线程存活
//（`_watcher` 若被丢弃，文件监听会随之停止——下划线前缀表示"仅作持有，不直接读"）。
#[derive(Resource)]
pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
    receiver: Receiver<Result<Event, notify::Error>>,
}

pub struct ConfigPlugin;

impl Plugin for ConfigPlugin {
    fn build(&self, app: &mut App) {
        // 先加载一份配置（失败会回退默认），随后插入为资源。
        let config = SimulationConfig::default();

        // Set up file watcher using crossbeam-channel for thread safety
        // 用 crossbeam 通道搭建文件监听器，保证跨线程安全（监听线程 send，主线程 recv）。
        let (tx, rx): (
            Sender<Result<Event, notify::Error>>,
            Receiver<Result<Event, notify::Error>>,
        ) = unbounded();
        let watcher_result = RecommendedWatcher::new(
            // 监听回调在后台线程执行：把事件塞进通道即可，不直接碰世界数据。
            move |res| {
                let _ = tx.send(res);
            },
            notify::Config::default(),
        );

        // `match` 处理创建监听器的成功/失败。
        match watcher_result {
            Ok(mut watcher) => {
                // `RecursiveMode::NonRecursive`：只监听 config.toml 本身，不递归目录。
                if let Err(e) = watcher.watch(Path::new("config.toml"), RecursiveMode::NonRecursive)
                {
                    warn!("Failed to watch config.toml: {}", e);
                } else {
                    info!("Config hot-reload enabled for config.toml");
                    // 把监听器与接收端存成资源，并注册热重载系统。
                    app.insert_resource(ConfigWatcher {
                        _watcher: watcher,
                        receiver: rx,
                    });
                    app.add_systems(Update, config_hot_reload);
                }
            }
            Err(e) => {
                warn!("Failed to create config watcher: {}", e);
            }
        }

        // 插入配置资源并注册其反射类型（供检视器/序列化使用）。
        app.insert_resource(config)
            .register_type::<SimulationConfig>();
    }
}

/// 每帧检查配置文件是否被改动；有则重新加载并就地更新。
/// 参数用 `Option<Res<...>>` / `Option<ResMut<...>>`：即使这些资源不存在（如无监听器、
/// 未跑物理）系统也能安全运行——取不到就跳过相应更新。
fn config_hot_reload(
    mut config: ResMut<SimulationConfig>,
    watcher: Option<Res<ConfigWatcher>>,
    mut substeps: Option<ResMut<SubstepCount>>,
    mut fixed_time: Option<ResMut<Time<Fixed>>>,
) {
    // 没有监听器资源就直接返回（热重载未启用）。
    let Some(watcher) = watcher else {
        return;
    };

    // Non-blocking check for file changes
    // 非阻塞地检查文件改动：`try_recv` 取不到就报 Err，`while let Ok(Ok(...))` 循环取直到通道空。
    while let Ok(Ok(event)) = watcher.receiver.try_recv() {
        // 只关心"修改"类事件（新建/重命名等忽略）。
        if event.kind.is_modify() {
            match SimulationConfig::load() {
                Ok(new_config) => {
                    info!("Config reloaded successfully");
                    // 物理子步数这类"引擎持有的资源"需要单独同步（不只是覆盖配置资源）。
                    if let Some(substeps) = substeps.as_deref_mut() {
                        substeps.0 = new_config.physics.substep_count;
                    }
                    // 固定时间步也随配置更新；`.max(1.0)` 防止 0 导致异常。
                    if let Some(fixed_time) = fixed_time.as_deref_mut() {
                        *fixed_time = Time::<Fixed>::from_hz(new_config.physics.fixed_hz.max(1.0));
                    }
                    // 最后整体替换配置资源（`*config` 解引用后赋值）。
                    *config = new_config;
                }
                Err(e) => {
                    warn!("Failed to reload config: {}", e);
                }
            }
        }
    }
}
