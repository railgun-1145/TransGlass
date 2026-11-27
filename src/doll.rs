//! MC 皮肤 3D 纸娃娃（基岩版左上角纸娃娃观感）
//!
//! 纯 CPU 光栅化，不依赖任何渲染后端，输出直通 alpha 的 RGBA8，
//! 直接喂给 `UpdateLayeredWindow` 的分层窗口缓冲。
//!
//! 实现要点：
//! - 6 个盒子（头 / 身体 / 双臂 / 双腿）从皮肤 UV 逐面取纹理；
//!   皮肤的 UV 版式就是 MC 的盒子展开图：右/前/左/后/顶/底。
//! - 透视投影 + 逐面背面剔除 + 画家算法（背面→正面）+ 深度缓冲。
//! - N×N 超采样后盒式滤波降采样，像素画边界依然干净（不会锯齿抖）。
//! - 世界空间顶光做轻微明暗（正面 0.90、侧面 0.84/0.91、顶面 0.99），
//!   不破坏皮肤原色，但能看出体积。
//! - 经典 / 纤细（Alex）两套 UV，第二层皮肤（帽子、外套、袖子）作为
//!   外扩 0.5 / 0.25 像素的壳层叠加。
//! - 头可以相对身体额外转头、低头，手臂 / 腿可以摆动。

use image::RgbaImage;

const TEX: f32 = 64.0; // 标准皮肤边长，HD 皮肤按比例缩放 UV

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ModelKind {
    Classic,
    Slim,
}

impl ModelKind {
    pub fn label(self) -> &'static str {
        match self {
            ModelKind::Classic => "宽（Steve）",
            ModelKind::Slim => "细（Alex）",
        }
    }
    pub fn parse(s: &str) -> Option<ModelKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "classic" | "wide" | "steve" => Some(ModelKind::Classic),
            "slim" | "alex" | "thin" => Some(ModelKind::Slim),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ModelKind::Classic => "classic",
            ModelKind::Slim => "slim",
        }
    }
}

/// 解析好的皮肤：像素拷贝成 [r,g,b,a]，UV 缩放系数按皮肤边长算。
pub struct Skin {
    w: i32,
    h: i32,
    px: Vec<[u8; 4]>,
    pub model: ModelKind,
    pub overlay: bool,
    uv_sx: f32,
    uv_sy: f32,
}

impl Skin {
    /// `forced` 为 None 时按手臂纹理区自动判定宽 / 细模型。
    pub fn from_rgba(img: &RgbaImage, forced: Option<ModelKind>) -> Skin {
        let (w, h) = (img.width() as i32, img.height() as i32);
        let mut px = Vec::with_capacity((w as usize) * (h as usize));
        for p in img.pixels() {
            px.push([p[0], p[1], p[2], p[3]]);
        }
        let model = forced.unwrap_or_else(|| detect_model(&px, w, h));
        let mut skin = Skin {
            w,
            h,
            px,
            model,
            overlay: false,
            uv_sx: w as f32 / TEX,
            uv_sy: h as f32 / TEX,
        };
        skin.overlay = has_overlay(&skin);
        skin
    }

    #[inline]
    fn texel(&self, x: i32, y: i32) -> [u8; 4] {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return [0, 0, 0, 0];
        }
        self.px[(y * self.w + x) as usize]
    }

    /// u / v 已经是皮肤像素坐标（还没取整）
    #[inline]
    fn texel_at(&self, u: f32, v: f32) -> [u8; 4] {
        self.texel(u.floor() as i32, v.floor() as i32)
    }

    pub fn aspect_ok(&self) -> bool {
        self.w >= 32 && self.h >= 16
    }
}

/// 细手臂判定：纤细皮肤的右臂背面只占 3 列，纹理区最右两列（54、55）空着。
fn detect_model(px: &[[u8; 4]], w: i32, h: i32) -> ModelKind {
    if w < 56 || h < 32 {
        return ModelKind::Classic;
    }
    let (mut content, mut unused) = (0u32, 0u32);
    for y in 20..32 {
        for x in 44..47 {
            if px[(y * w + x) as usize][3] > 8 {
                content += 1;
            }
        }
        for x in 54..56 {
            if px[(y * w + x) as usize][3] > 8 {
                unused += 1;
            }
        }
    }
    if content > 0 && unused == 0 {
        ModelKind::Slim
    } else {
        ModelKind::Classic
    }
}

#[derive(Clone, Copy, Default)]
struct V3 {
    x: f32,
    y: f32,
    z: f32,
}

impl V3 {
    const fn new(x: f32, y: f32, z: f32) -> Self {
        V3 { x, y, z }
    }
    const ZERO: V3 = V3::new(0.0, 0.0, 0.0);
    #[inline]
    fn add(self, o: V3) -> V3 {
        V3::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
    #[inline]
    fn sub(self, o: V3) -> V3 {
        V3::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
    #[inline]
    fn mul(self, s: f32) -> V3 {
        V3::new(self.x * s, self.y * s, self.z * s)
    }
    #[inline]
    fn dot(self, o: V3) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }
    #[inline]
    fn cross(self, o: V3) -> V3 {
        V3::new(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }
    /// 绕 X 轴：y' = y cos - z sin，z' = y sin + z cos
    #[inline]
    fn rot_x(self, a: f32) -> V3 {
        let (s, c) = (a.sin(), a.cos());
        V3::new(self.x, self.y * c - self.z * s, self.y * s + self.z * c)
    }
    /// 绕 Y 轴：x' = x cos + z sin，z' = -x sin + z cos
    #[inline]
    fn rot_y(self, a: f32) -> V3 {
        let (s, c) = (a.sin(), a.cos());
        V3::new(self.x * c + self.z * s, self.y, -self.x * s + self.z * c)
    }
}

/// 皮肤盒子的 UV 原点（64×64 版式单位）
#[derive(Clone, Copy)]
struct UvBox {
    u0: f32,
    v0: f32,
    w: f32,
    h: f32,
    d: f32,
}

#[derive(Clone, Copy)]
struct Rect {
    u: f32,
    v: f32,
    w: f32,
    h: f32,
}

impl UvBox {
    /// 0 右(-x) 1 前(+z) 2 左(+x) 3 后(-z) 4 顶(+y) 5 底(-y)
    fn face(&self, kind: usize) -> Rect {
        let (u0, v0, w, h, d) = (self.u0, self.v0, self.w, self.h, self.d);
        match kind {
            0 => Rect { u: u0, v: v0 + d, w: d, h },
            1 => Rect { u: u0 + d, v: v0 + d, w, h },
            2 => Rect { u: u0 + d + w, v: v0 + d, w: d, h },
            3 => Rect { u: u0 + 2.0 * d + w, v: v0 + d, w, h },
            4 => Rect { u: u0 + d, v: v0, w, h: d },
            _ => Rect { u: u0 + d + w, v: v0, w, h: d },
        }
    }
}

#[derive(Clone, Copy)]
struct Quad {
    p: [V3; 4],
    uv: [(f32, f32); 4],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pivot {
    None,
    Head,
    ArmR,
    ArmL,
    LegR,
    LegL,
}

struct PartDef {
    center: V3,
    half: V3,
    uv: UvBox,
    pivot: V3,
    pivot_kind: Pivot,
}

/// 12 个盒子：前 6 个基础层，后 6 个第二层（皮肤没有第二层内容时跳过）
fn part_defs(skin: &Skin) -> Vec<PartDef> {
    let slim = skin.model == ModelKind::Slim;
    let aw = if slim { 3.0 } else { 4.0 };
    let ax = 4.0 + aw * 0.5; // 手臂中心到身体中轴
    let (head_c, neck) = (V3::new(0.0, 28.0, 0.0), V3::new(0.0, 24.0, 0.0));
    let (arm_r, arm_l) = (V3::new(-ax, 18.0, 0.0), V3::new(ax, 18.0, 0.0));
    let shoulder = V3::new(0.0, 6.0, 0.0); // 手臂中心到肩膀支点
    let hip = V3::new(0.0, 6.0, 0.0); // 腿中心到胯支点
    let (leg_r, leg_l) = (V3::new(-2.0, 6.0, 0.0), V3::new(2.0, 6.0, 0.0));
    let body_c = V3::new(0.0, 18.0, 0.0);
    let half_arm = V3::new(aw * 0.5, 6.0, 2.0);
    let half_leg = V3::new(2.0, 6.0, 2.0);

    let mut out = Vec::with_capacity(12);
    // ---- 基础层 ----
    out.push(PartDef {
        center: head_c,
        half: V3::new(4.0, 4.0, 4.0),
        uv: UvBox { u0: 0.0, v0: 0.0, w: 8.0, h: 8.0, d: 8.0 },
        pivot: neck,
        pivot_kind: Pivot::Head,
    });
    out.push(PartDef {
        center: body_c,
        half: V3::new(4.0, 6.0, 2.0),
        uv: UvBox { u0: 16.0, v0: 16.0, w: 8.0, h: 12.0, d: 4.0 },
        pivot: V3::ZERO,
        pivot_kind: Pivot::None,
    });
    out.push(PartDef {
        center: arm_r,
        half: half_arm,
        uv: UvBox { u0: 40.0, v0: 16.0, w: aw, h: 12.0, d: 4.0 },
        pivot: arm_r.add(shoulder),
        pivot_kind: Pivot::ArmR,
    });
    out.push(PartDef {
        center: arm_l,
        half: half_arm,
        uv: UvBox { u0: 32.0, v0: 48.0, w: aw, h: 12.0, d: 4.0 },
        pivot: arm_l.add(shoulder),
        pivot_kind: Pivot::ArmL,
    });
    out.push(PartDef {
        center: leg_r,
        half: half_leg,
        uv: UvBox { u0: 0.0, v0: 16.0, w: 4.0, h: 12.0, d: 4.0 },
        pivot: leg_r.add(hip),
        pivot_kind: Pivot::LegR,
    });
    out.push(PartDef {
        center: leg_l,
        half: half_leg,
        uv: UvBox { u0: 16.0, v0: 48.0, w: 4.0, h: 12.0, d: 4.0 },
        pivot: leg_l.add(hip),
        pivot_kind: Pivot::LegL,
    });

    // ---- 第二层皮肤：帽子外扩 0.5，其余 0.25 ----
    let ih = 0.5;
    let ib = 0.25;
    out.push(PartDef {
        center: head_c,
        half: V3::new(4.0 + ih, 4.0 + ih, 4.0 + ih),
        uv: UvBox { u0: 32.0, v0: 0.0, w: 8.0, h: 8.0, d: 8.0 },
        pivot: neck,
        pivot_kind: Pivot::Head,
    });
    out.push(PartDef {
        center: body_c,
        half: V3::new(4.0 + ib, 6.0 + ib, 2.0 + ib),
        uv: UvBox { u0: 16.0, v0: 32.0, w: 8.0, h: 12.0, d: 4.0 },
        pivot: V3::ZERO,
        pivot_kind: Pivot::None,
    });
    out.push(PartDef {
        center: arm_r,
        half: V3::new(aw * 0.5 + ib, 6.0 + ib, 2.0 + ib),
        uv: UvBox { u0: 40.0, v0: 32.0, w: aw, h: 12.0, d: 4.0 },
        pivot: arm_r.add(shoulder),
        pivot_kind: Pivot::ArmR,
    });
    out.push(PartDef {
        center: arm_l,
        half: V3::new(aw * 0.5 + ib, 6.0 + ib, 2.0 + ib),
        uv: UvBox { u0: 48.0, v0: 48.0, w: aw, h: 12.0, d: 4.0 },
        pivot: arm_l.add(shoulder),
        pivot_kind: Pivot::ArmL,
    });
    out.push(PartDef {
        center: leg_r,
        half: V3::new(2.0 + ib, 6.0 + ib, 2.0 + ib),
        uv: UvBox { u0: 0.0, v0: 32.0, w: 4.0, h: 12.0, d: 4.0 },
        pivot: leg_r.add(hip),
        pivot_kind: Pivot::LegR,
    });
    out.push(PartDef {
        center: leg_l,
        half: V3::new(2.0 + ib, 6.0 + ib, 2.0 + ib),
        uv: UvBox { u0: 0.0, v0: 48.0, w: 4.0, h: 12.0, d: 4.0 },
        pivot: leg_l.add(hip),
        pivot_kind: Pivot::LegL,
    });
    out
}

/// 第二层皮肤是否有内容（没有就整层跳过，省一半像素）
fn has_overlay(skin: &Skin) -> bool {
    if skin.w < 64 || skin.h < 64 {
        return false;
    }
    for d in part_defs(skin).iter().skip(6) {
        for kind in 0..6 {
            let rc = d.uv.face(kind);
            let x0 = (rc.u * skin.uv_sx).floor() as i32;
            let y0 = (rc.v * skin.uv_sy).floor() as i32;
            let x1 = ((rc.u + rc.w) * skin.uv_sx).ceil() as i32;
            let y1 = ((rc.v + rc.h) * skin.uv_sy).ceil() as i32;
            for y in y0..y1 {
                for x in x0..x1 {
                    if skin.texel(x, y)[3] > 8 {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// 一个盒子的 4 个角（相对盒子中心），顺序 = 从外侧看 左上→右上→右下→左下
fn face_corners(kind: usize, hx: f32, hy: f32, hz: f32) -> [V3; 4] {
    match kind {
        0 => [
            V3::new(-hx, hy, -hz),
            V3::new(-hx, hy, hz),
            V3::new(-hx, -hy, hz),
            V3::new(-hx, -hy, -hz),
        ],
        1 => [
            V3::new(-hx, hy, hz),
            V3::new(hx, hy, hz),
            V3::new(hx, -hy, hz),
            V3::new(-hx, -hy, hz),
        ],
        2 => [
            V3::new(hx, hy, hz),
            V3::new(hx, hy, -hz),
            V3::new(hx, -hy, -hz),
            V3::new(hx, -hy, hz),
        ],
        3 => [
            V3::new(hx, hy, -hz),
            V3::new(-hx, hy, -hz),
            V3::new(-hx, -hy, -hz),
            V3::new(hx, -hy, -hz),
        ],
        4 => [
            V3::new(-hx, hy, -hz),
            V3::new(hx, hy, -hz),
            V3::new(hx, hy, hz),
            V3::new(-hx, hy, hz),
        ],
        _ => [
            V3::new(-hx, -hy, hz),
            V3::new(hx, -hy, hz),
            V3::new(hx, -hy, -hz),
            V3::new(-hx, -hy, -hz),
        ],
    }
}

/// 相机 / 姿态。角度单位都是弧度。
#[derive(Clone, Copy)]
pub struct Pose {
    /// 身体朝向：0 = 正面朝观察者，正 = 向右转（模型正面转向 +x）
    pub yaw: f32,
    /// 相机俯角（正 = 从上往下看）
    pub cam_pitch: f32,
    /// 头相对身体额外转的角度（正 = 向 +x 侧转）
    pub head_yaw: f32,
    /// 头俯仰（正 = 低头）
    pub head_pitch: f32,
    /// 手臂摆动：正 = 右臂向前
    pub arm_swing: f32,
    /// 腿摆动：正 = 右腿向前
    pub leg_swing: f32,
    /// 每个皮肤像素对应的屏幕像素
    pub scale: f32,
    /// 相机距离（皮肤像素单位），越小透视越强
    pub dist: f32,
    /// 超采样倍数（1..=4）
    pub ss: u8,
}

impl Default for Pose {
    fn default() -> Self {
        Pose {
            yaw: 0.0,
            cam_pitch: 18.0f32.to_radians(),
            head_yaw: 0.0,
            head_pitch: 0.0,
            arm_swing: 0.0,
            leg_swing: 0.0,
            scale: 2.5,
            dist: 95.0,
            ss: 3,
        }
    }
}

/// 世界空间顶光（略偏观察者左侧），只用来给出体积感的极轻微明暗
const LIGHT: V3 = V3::new(-0.351, 0.903, 0.251);

fn push_box(out: &mut Vec<Quad>, d: &PartDef, rx: f32, ry: f32, uv_sx: f32, uv_sy: f32) {
    for kind in 0..6 {
        let rc = d.uv.face(kind);
        let corners = face_corners(kind, d.half.x, d.half.y, d.half.z);
        // 直接乘上 UV 缩放，光栅化时省掉两次乘法
        let (u0, v0) = (rc.u * uv_sx, rc.v * uv_sy);
        let (uw, vh) = (rc.w * uv_sx, rc.h * uv_sy);
        let uvs = [(u0, v0), (u0 + uw, v0), (u0 + uw, v0 + vh), (u0, v0 + vh)];
        let mut p = [V3::ZERO; 4];
        for i in 0..4 {
            let mut v = d.center.add(corners[i]).sub(d.pivot);
            if rx != 0.0 {
                v = v.rot_x(rx);
            }
            if ry != 0.0 {
                v = v.rot_y(ry);
            }
            p[i] = v.add(d.pivot);
        }
        out.push(Quad { p, uv: uvs });
    }
}

fn build_quads(skin: &Skin, pose: &Pose, out: &mut Vec<Quad>) {
    let (sx, sy) = (skin.uv_sx, skin.uv_sy);
    for d in part_defs(skin) {
        let (rx, ry) = match d.pivot_kind {
            Pivot::None => (0.0, 0.0),
            Pivot::Head => (pose.head_pitch, pose.head_yaw),
            Pivot::ArmR => (pose.arm_swing, 0.0),
            Pivot::ArmL => (-pose.arm_swing, 0.0),
            Pivot::LegR => (pose.leg_swing, 0.0),
            Pivot::LegL => (-pose.leg_swing, 0.0),
        };
        push_box(out, &d, rx, ry, sx, sy);
    }
}

struct ProjQuad {
    s: [(f32, f32); 4], // 屏幕坐标（目标像素单位）
    ivz: [f32; 4],      // 1/深度
    uvz: [(f32, f32); 4], // u/z, v/z（u、v 已是皮肤像素坐标）
    depth: f32,         // 平均深度，用于排序
    shade: f32,         // 该面的明暗系数
}

/// 直通 alpha 的 over 合成。目标是 BGRA（Windows 分层窗口 DIB 的原生字节序）。
#[inline]
fn over(dst: &mut [u8], o: usize, r: f32, g: f32, b: f32, a: f32) {
    if a <= 0.0005 {
        return;
    }
    let da = dst[o + 3] as f32 / 255.0;
    let oa = a + da * (1.0 - a);
    if oa <= 0.002 {
        return;
    }
    let k = 1.0 / oa;
    dst[o] = (((b * a + dst[o] as f32 / 255.0 * da * (1.0 - a)) * k) * 255.0).clamp(0.0, 255.0) as u8;
    dst[o + 1] =
        (((g * a + dst[o + 1] as f32 / 255.0 * da * (1.0 - a)) * k) * 255.0).clamp(0.0, 255.0) as u8;
    dst[o + 2] =
        (((r * a + dst[o + 2] as f32 / 255.0 * da * (1.0 - a)) * k) * 255.0).clamp(0.0, 255.0) as u8;
    dst[o + 3] = (oa * 255.0) as u8;
}

/// 把纸娃娃画到 `dst`（BGRA8 + 直通 alpha，行优先，即 Windows DIB 的字节序）。
/// (cx, cy) 是模型身体中心的屏幕位置。返回实际绘制的包围盒 [x0,y0,x1,y1]（右/下开区间），
/// 包围盒已包含 1px 描边。
pub fn render(
    dst: &mut [u8],
    dw: usize,
    dh: usize,
    cx: f32,
    cy: f32,
    skin: &Skin,
    pose: &Pose,
) -> Option<(i32, i32, i32, i32)> {
    if !skin.aspect_ok() {
        return None;
    }
    let mut quads: Vec<Quad> = Vec::with_capacity(96);
    build_quads(skin, pose, &mut quads);

    let yaw = pose.yaw;
    let (syaw, cyaw) = (yaw.sin(), yaw.cos());
    let cp = pose.cam_pitch.cos();
    let sp = pose.cam_pitch.sin();
    let dist = pose.dist.max(20.0);
    let f = pose.scale * dist;

    // 模型空间 -> 相机空间（先绕 Y 转到朝向，再俯仰，最后把模型中心移到原点）
    let to_cam = |v: V3| -> V3 {
        let x1 = v.x * cyaw + v.z * syaw;
        let z1 = -v.x * syaw + v.z * cyaw;
        let y1 = v.y - 16.0;
        V3::new(x1, y1 * cp - z1 * sp, y1 * sp + z1 * cp)
    };

    let mut vis: Vec<ProjQuad> = Vec::with_capacity(quads.len());
    let (mut minx, mut miny) = (f32::MAX, f32::MAX);
    let (mut maxx, mut maxy) = (f32::MIN, f32::MIN);
    let mut any = false;
    for q in &quads {
        let mut cam = [V3::ZERO; 4];
        for i in 0..4 {
            cam[i] = to_cam(q.p[i]);
        }
        // 外法线（顶点顺序是外侧顺时针，叉积得到内法线，取反）
        let n = cam[1].sub(cam[0]).cross(cam[2].sub(cam[0])).mul(-1.0);
        let to_camera = V3::new(0.0, 0.0, dist).sub(cam[0]);
        if n.dot(to_camera) <= 0.0 {
            continue; // 背面
        }
        // 世界空间顶光：正面 ≈0.90、侧面 ≈0.84/0.91、顶面 ≈0.99、底面 ≈0.83
        let len = n.dot(n).sqrt().max(1e-6);
        let shade = 0.86 + 0.14 * (n.dot(LIGHT) / len).clamp(-0.25, 1.0);
        let mut s = [(0f32, 0f32); 4];
        let mut ivz = [0f32; 4];
        let mut uvz = [(0f32, 0f32); 4];
        let mut depth_sum = 0.0;
        let mut ok = true;
        for i in 0..4 {
            let depth = dist - cam[i].z;
            if depth < 2.0 {
                ok = false;
                break;
            }
            let iz = 1.0 / depth;
            s[i] = (f * cam[i].x * iz + cx, -f * cam[i].y * iz + cy);
            ivz[i] = iz;
            uvz[i] = (q.uv[i].0 * iz, q.uv[i].1 * iz);
            depth_sum += depth;
        }
        if !ok {
            continue;
        }
        any = true;
        for i in 0..4 {
            minx = minx.min(s[i].0);
            maxx = maxx.max(s[i].0);
            miny = miny.min(s[i].1);
            maxy = maxy.max(s[i].1);
        }
        vis.push(ProjQuad { s, ivz, uvz, depth: depth_sum * 0.25, shade });
    }
    if !any {
        return None;
    }

    // 包围盒（含 1px 余量），裁剪到目标
    let x0 = (minx.floor() as i32 - 1).clamp(0, dw as i32);
    let y0 = (miny.floor() as i32 - 1).clamp(0, dh as i32);
    let x1 = (maxx.ceil() as i32 + 1).clamp(0, dw as i32);
    let y1 = (maxy.ceil() as i32 + 1).clamp(0, dh as i32);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let bw = (x1 - x0) as usize;
    let bh = (y1 - y0) as usize;
    let ss = pose.ss.clamp(1, 4) as usize;
    let sw = bw * ss;
    let sh = bh * ss;
    if sw * sh > 4_000_000 {
        return None; // 防御：正常尺寸远小于此
    }

    // 超采样缓冲：预乘 rgb + alpha，另存 1/深度
    let mut col = vec![[0f32; 4]; sw * sh];
    let mut zb = vec![0f32; sw * sh];
    let ssf = ss as f32;
    let step = 1.0 / ssf; // 一个 ss 采样在目标像素坐标里的步长

    // 画家算法：远的先画
    vis.sort_by(|a, b| b.depth.partial_cmp(&a.depth).unwrap_or(std::cmp::Ordering::Equal));

    for q in &vis {
        let shade = q.shade;
        for tri in [[0usize, 1, 2], [0, 2, 3]] {
            let (i0, i1, i2) = (tri[0], tri[1], tri[2]);
            let (ax, ay) = q.s[i0];
            let (bx, by) = q.s[i1];
            let (cx_, cy_) = q.s[i2];
            let area = (bx - ax) * (cy_ - ay) - (by - ay) * (cx_ - ax);
            if area.abs() < 1e-6 {
                continue;
            }
            let inv_area = 1.0 / area;
            let tx0 = (ax.min(bx).min(cx_) - x0 as f32) * ssf;
            let tx1 = (ax.max(bx).max(cx_) - x0 as f32) * ssf;
            let ty0 = (ay.min(by).min(cy_) - y0 as f32) * ssf;
            let ty1 = (ay.max(by).max(cy_) - y0 as f32) * ssf;
            let px0 = tx0.floor().max(0.0) as usize;
            let px1 = ((tx1.ceil() as i32 + 1).max(0) as usize).min(sw);
            let py0 = ty0.floor().max(0.0) as usize;
            let py1 = ((ty1.ceil() as i32 + 1).max(0) as usize).min(sh);
            if px1 <= px0 || py1 <= py0 {
                continue;
            }
            let (iv0, iv1, iv2) = (q.ivz[i0], q.ivz[i1], q.ivz[i2]);
            let (u0z, v0z) = q.uvz[i0];
            let (u1z, v1z) = q.uvz[i1];
            let (u2z, v2z) = q.uvz[i2];
            // 重心坐标 w0 / w1 对屏幕 x 的步进
            let dw0 = (by - cy_) * inv_area * step;
            let dw1 = (cy_ - ay) * inv_area * step;
            for py in py0..py1 {
                let ty = y0 as f32 + (py as f32 + 0.5) * step;
                let tx_first = x0 as f32 + (px0 as f32 + 0.5) * step;
                let mut w0 = ((bx - tx_first) * (cy_ - ty) - (by - ty) * (cx_ - tx_first)) * inv_area;
                let mut w1 = ((cx_ - tx_first) * (ay - ty) - (cy_ - ty) * (ax - tx_first)) * inv_area;
                let row = py * sw;
                for px in px0..px1 {
                    let w2 = 1.0 - w0 - w1;
                    if w0 >= -0.0005 && w1 >= -0.0005 && w2 >= -0.0005 {
                        let iz = w0 * iv0 + w1 * iv1 + w2 * iv2;
                        if iz > 0.0 {
                            let si = row + px;
                            if iz + 1e-7 >= zb[si] {
                                let ir = 1.0 / iz;
                                let u = (w0 * u0z + w1 * u1z + w2 * u2z) * ir;
                                let v = (w0 * v0z + w1 * v1z + w2 * v2z) * ir;
                                let tex = skin.texel_at(u, v);
                                if tex[3] >= 6 {
                                    let a = tex[3] as f32 / 255.0;
                                    let c = &mut col[si];
                                    // 预乘 alpha 合成（画家算法已保证从远到近）
                                    let sh = shade * a;
                                    c[0] = tex[0] as f32 / 255.0 * sh + c[0] * (1.0 - a);
                                    c[1] = tex[1] as f32 / 255.0 * sh + c[1] * (1.0 - a);
                                    c[2] = tex[2] as f32 / 255.0 * sh + c[2] * (1.0 - a);
                                    c[3] = a + c[3] * (1.0 - a);
                                    if a > 0.98 {
                                        zb[si] = iz;
                                    }
                                }
                            }
                        }
                    }
                    w0 += dw0;
                    w1 += dw1;
                }
            }
        }
    }

    // ---- 去掉孤立的覆盖杂点 ----
    // 光栅化/皮肤采样偶尔会在轮廓外留下 1~2 个子采样的小飞点（皮肤纹理在面边界
    // 取整时尤其明显）。3×3 超采样下它们几乎看不见，但描边会沿它们膨胀一圈，
    // 放大成后脑勺外面的"黑刺"。这里把小于 12 个子采样的连通块直接清掉。
    {
        const MIN_COMPONENT: usize = 12;
        let n = sw * sh;
        let mut seen = vec![false; n];
        let mut stack: Vec<usize> = Vec::new();
        let mut comp: Vec<usize> = Vec::new();
        for start in 0..n {
            if seen[start] || col[start][3] <= 0.002 {
                continue;
            }
            stack.push(start);
            seen[start] = true;
            comp.clear();
            while let Some(i) = stack.pop() {
                comp.push(i);
                let x = i % sw;
                let y = i / sw;
                // 4 邻域就够了：飞点是孤立的，模型本体是实心的
                if x > 0 {
                    let k = i - 1;
                    if !seen[k] && col[k][3] > 0.002 {
                        seen[k] = true;
                        stack.push(k);
                    }
                }
                if x + 1 < sw {
                    let k = i + 1;
                    if !seen[k] && col[k][3] > 0.002 {
                        seen[k] = true;
                        stack.push(k);
                    }
                }
                if y > 0 {
                    let k = i - sw;
                    if !seen[k] && col[k][3] > 0.002 {
                        seen[k] = true;
                        stack.push(k);
                    }
                }
                if y + 1 < sh {
                    let k = i + sw;
                    if !seen[k] && col[k][3] > 0.002 {
                        seen[k] = true;
                        stack.push(k);
                    }
                }
            }
            if comp.len() < MIN_COMPONENT {
                for &i in &comp {
                    col[i] = [0.0; 4];
                }
            }
        }
    }

    // ---- 1 目标像素描边：在超采样域膨胀覆盖遮罩，再跟模型一起降采样 ----
    //
    // 旧版在降采样后的 mask 上取 8 邻域最大值、alpha = 0.42 × 邻居覆盖，
    // 画出来是一条 1~2px、透明度忽高忽低的灰圈，缩到 3px/皮肤像素时就是"毛边"。
    // 这里改成对覆盖遮罩做半径 = ss 个子采样（正好 1 个目标像素）的最大值膨胀，
    // 只在模型没盖住的样本上铺描边色，最后由同一套超采样降采样出抗锯齿边缘。
    {
        let r = ss;
        let mut dil = vec![0f32; sw * sh];
        let mut tmp = vec![0f32; sw * sh];
        // 水平方向
        for y in 0..sh {
            let row = y * sw;
            for x in 0..sw {
                let lo = x.saturating_sub(r);
                let hi = (x + r + 1).min(sw);
                let mut m = 0.0f32;
                for xx in lo..hi {
                    let v = col[row + xx][3];
                    if v > m {
                        m = v;
                        if m >= 1.0 {
                            break;
                        }
                    }
                }
                tmp[row + x] = m;
            }
        }
        // 垂直方向
        for x in 0..sw {
            for y in 0..sh {
                let lo = y.saturating_sub(r);
                let hi = (y + r + 1).min(sh);
                let mut m = 0.0f32;
                for yy in lo..hi {
                    let v = tmp[yy * sw + x];
                    if v > m {
                        m = v;
                        if m >= 1.0 {
                            break;
                        }
                    }
                }
                dil[y * sw + x] = m;
            }
        }
        // 描边色（预乘），只在模型外圈；0.85 保证边缘是"深色线"而不是一层灰雾
        let (cr, cg, cb) = (0.031f32, 0.039f32, 0.055f32);
        for i in 0..sw * sh {
            let c = &mut col[i];
            if c[3] > 0.002 {
                continue;
            }
            let a = 0.85 * dil[i].clamp(0.0, 1.0);
            if a <= 0.0 {
                continue;
            }
            c[0] = cr * a;
            c[1] = cg * a;
            c[2] = cb * a;
            c[3] = a;
        }
    }

    // 盒式滤波降采样
    let inv_n = 1.0 / (ss * ss) as f32;
    for j in 0..bh {
        for i in 0..bw {
            let mut acc = [0f32; 4];
            for sj in 0..ss {
                let row = (j * ss + sj) * sw + i * ss;
                for si in 0..ss {
                    let c = col[row + si];
                    if c[3] <= 0.0 {
                        continue;
                    }
                    acc[0] += c[0];
                    acc[1] += c[1];
                    acc[2] += c[2];
                    acc[3] += c[3];
                }
            }
            if acc[3] <= 0.0 {
                continue;
            }
            let a = (acc[3] * inv_n).clamp(0.0, 1.0);
            let o = (((y0 + j as i32) * dw as i32 + (x0 + i as i32)) * 4) as usize;
            over(
                dst,
                o,
                (acc[0] / acc[3]).clamp(0.0, 1.0),
                (acc[1] / acc[3]).clamp(0.0, 1.0),
                (acc[2] / acc[3]).clamp(0.0, 1.0),
                a,
            );
        }
    }
    Some((x0 - 1, y0 - 1, x1 + 1, y1 + 1))
}
