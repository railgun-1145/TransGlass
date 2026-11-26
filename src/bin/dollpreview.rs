// 开发用：把 3D 纸娃娃渲染到 PNG，检查 UV / 姿态 / 抗锯齿。
// 用法：cargo run --release --bin dollpreview -- [skin.png] [out.png] [--classic|--slim] [--scale=5] [--yaw=0,45] [--pitch=12] [--dist=62] [--cell=200] [--ss=3]
#[path = "../doll.rs"]
mod doll;

use doll::{ModelKind, Pose, Skin};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut forced: Option<ModelKind> = None;
    let mut scale = 5.0f32;
    let mut ss = 3u8;
    let mut yaw_list: Vec<f32> = vec![-90.0, -45.0, 0.0, 45.0, 90.0, 135.0, 180.0, -135.0];
    let mut cell = 200u32;
    let mut pitch = 12.0f32;
    let mut dist = 62.0f32;
    let mut head = (0.0f32, 0.0f32);
    for a in &args {
        if a == "--classic" {
            forced = Some(ModelKind::Classic);
        } else if a == "--slim" {
            forced = Some(ModelKind::Slim);
        } else if let Some(v) = a.strip_prefix("--scale=") {
            scale = v.parse().unwrap_or(5.0);
        } else if let Some(v) = a.strip_prefix("--ss=") {
            ss = v.parse().unwrap_or(3);
        } else if let Some(v) = a.strip_prefix("--yaw=") {
            yaw_list = v.split(',').filter_map(|s| s.parse().ok()).collect();
        } else if let Some(v) = a.strip_prefix("--cell=") {
            cell = v.parse().unwrap_or(200);
        } else if let Some(v) = a.strip_prefix("--pitch=") {
            pitch = v.parse().unwrap_or(12.0);
        } else if let Some(v) = a.strip_prefix("--dist=") {
            dist = v.parse().unwrap_or(62.0);
        } else if let Some(v) = a.strip_prefix("--head=") {
            let mut it = v.split(',');
            let hy: f32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let hp: f32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            head = (hy, hp);
        } else {
            positional.push(a.clone());
        }
    }
    let skin_path = positional
        .first()
        .cloned()
        .unwrap_or_else(|| "skins/steve.png".into());
    let out = positional
        .get(1)
        .cloned()
        .unwrap_or_else(|| "doll_preview.png".into());

    let img = match image::open(&skin_path) {
        Ok(i) => i.to_rgba8(),
        Err(e) => {
            eprintln!("打开皮肤失败 {skin_path}: {e}");
            std::process::exit(1);
        }
    };
    let skin = Skin::from_rgba(&img, forced);
    println!(
        "skin={skin_path} {}x{} model={:?} overlay={}",
        img.width(),
        img.height(),
        skin.model,
        skin.overlay
    );

    let cw = cell;
    let ch = cell;
    let mut canvas = image::RgbaImage::new(cw * yaw_list.len() as u32, ch);
    // 棋盘背景，方便看透明边缘（浅灰两档，避免和皮肤色混淆）
    for y in 0..ch {
        for x in 0..canvas.width() {
            let on = ((x / 8) + (y / 8)) % 2 == 0;
            let v = if on { 226 } else { 196 };
            canvas.put_pixel(x, y, image::Rgba([v, v, v, 255]));
        }
    }

    for (i, yaw) in yaw_list.iter().enumerate() {
        let mut buf = vec![0u8; (cw * ch * 4) as usize];
        for y in 0..ch {
            for x in 0..cw {
                let p = canvas.get_pixel(i as u32 * cw + x, y);
                let o = ((y * cw + x) * 4) as usize;
                buf[o] = p[0];
                buf[o + 1] = p[1];
                buf[o + 2] = p[2];
                buf[o + 3] = 255;
            }
        }
        let pose = Pose {
            yaw: yaw.to_radians(),
            cam_pitch: pitch.to_radians(),
            head_yaw: head.0.to_radians(),
            head_pitch: head.1.to_radians(),
            arm_swing: 4.0f32.to_radians(),
            leg_swing: 0.0,
            scale,
            dist,
            ss,
        };
        let t0 = std::time::Instant::now();
        let bbox = doll::render(
            &mut buf,
            cw as usize,
            ch as usize,
            cw as f32 / 2.0,
            ch as f32 / 2.0,
            &skin,
            &pose,
        );
        let us = t0.elapsed().as_micros();
        println!("yaw={yaw:>7.1} bbox={bbox:?} {us}us");
        for y in 0..ch {
            for x in 0..cw {
                let o = ((y * cw + x) * 4) as usize;
                // doll::render 输出 BGRA
                canvas.put_pixel(
                    i as u32 * cw + x,
                    y,
                    image::Rgba([buf[o + 2], buf[o + 1], buf[o], 255]),
                );
            }
        }
    }
    canvas.save(&out).expect("保存预览失败");
    println!("写出 {out}");
}
