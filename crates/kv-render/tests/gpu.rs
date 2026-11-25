//! GPU compositor parity with the CPU compositor (ADR 0009).
//!
//! Every test that needs a device skips with a `SKIPPED: no GPU adapter`
//! line when none is found (Linux CI uses lavapipe when it is installed).
#![cfg(feature = "gpu")]

use kv_core::Micros;
use kv_media::VideoFrame;
use kv_render::gpu::{GpuCompositor, GpuContext, GpuError, WgpuContext};
use kv_render::{composite, source_size_for, Fit};
use kv_testkit::{render_tagged_frame, FrameTag};

/// Allowed per-channel difference between GPU and CPU output.
const TOLERANCE: u8 = 3;

fn ctx() -> Option<WgpuContext> {
    match WgpuContext::headless() {
        Ok(c) => {
            let info = c.adapter_info();
            println!(
                "GPU adapter: {} ({:?}, {:?})",
                info.name, info.backend, info.device_type
            );
            Some(c)
        }
        Err(e) => {
            println!("SKIPPED: no GPU adapter ({e})");
            None
        }
    }
}

fn tagged(clip: u8, frame: u16, w: u32, h: u32) -> VideoFrame {
    let data = render_tagged_frame(FrameTag { clip, frame }, w, h);
    VideoFrame::new_rgba(Micros(0), w, h, data).unwrap()
}

/// Deterministic noise: the worst case for resampling differences.
fn noise(w: u32, h: u32, seed: u32) -> VideoFrame {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    let data = (0..w * h * 4)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8
        })
        .collect();
    VideoFrame::new_rgba(Micros(0), w, h, data).unwrap()
}

fn cpu(layers: &[(&VideoFrame, f32)], w: u32, h: u32, fit: Fit) -> VideoFrame {
    let mut out = VideoFrame::solid(Micros(0), w, h, [0, 0, 0, 0]);
    composite(layers, &mut out, fit).unwrap();
    out
}

fn gpu(ctx: &WgpuContext, layers: &[(&VideoFrame, f32)], w: u32, h: u32, fit: Fit) -> VideoFrame {
    let mut c = GpuCompositor::new(ctx, w, h).unwrap();
    let ids: Vec<_> = layers
        .iter()
        .map(|&(f, weight)| (c.upload(f).unwrap(), weight))
        .collect();
    c.render(&ids, fit).unwrap();
    c.read_back().unwrap()
}

/// Covered output rectangle of a layer, as the compositors place it.
fn covered(src_w: u32, src_h: u32, w: u32, h: u32, fit: Fit) -> [i64; 4] {
    let (sw, sh) = source_size_for(w, h, src_w, src_h, fit);
    let ox = (i64::from(w) - i64::from(sw)).div_euclid(2);
    let oy = (i64::from(h) - i64::from(sh)).div_euclid(2);
    [
        ox.max(0),
        (ox + i64::from(sw)).min(i64::from(w)),
        oy.max(0),
        (oy + i64::from(sh)).min(i64::from(h)),
    ]
}

/// Assert GPU and CPU output agree within [`TOLERANCE`], except within
/// 1 px of a layer's crop edges (the borders of its covered rectangle).
fn assert_parity(layers: &[(&VideoFrame, f32)], w: u32, h: u32, fit: Fit, what: &str) {
    let Some(ctx) = ctx() else { return };
    let want = cpu(layers, w, h, fit);
    let got = gpu(&ctx, layers, w, h, fit);
    assert_eq!((got.width, got.height), (w, h));
    let rects: Vec<[i64; 4]> = layers
        .iter()
        .map(|(f, _)| covered(f.width, f.height, w, h, fit))
        .collect();
    let near_edge = |x: i64, y: i64| {
        rects.iter().any(|&[x0, x1, y0, y1]| {
            let near = |v: i64, e: i64| (v - e).abs() <= 1 || (v - (e - 1)).abs() <= 1;
            let edge_x = (near(x, x0) && x0 > 0) || (near(x, x1) && x1 < i64::from(w));
            let edge_y = (near(y, y0) && y0 > 0) || (near(y, y1) && y1 < i64::from(h));
            edge_x || edge_y
        })
    };
    let mut max = 0u8;
    let mut skipped = 0usize;
    for y in 0..i64::from(h) {
        for x in 0..i64::from(w) {
            let i = ((y * i64::from(w) + x) * 4) as usize;
            if near_edge(x, y) {
                skipped += 1;
                continue;
            }
            for c in 0..4 {
                let d = got.data[i + c].abs_diff(want.data[i + c]);
                assert!(
                    d <= TOLERANCE,
                    "{what}: pixel ({x}, {y}) channel {c}: gpu {} cpu {}",
                    got.data[i + c],
                    want.data[i + c]
                );
                max = max.max(d);
            }
        }
    }
    println!("{what}: max channel difference {max}, {skipped} edge pixels excluded");
}

#[test]
fn single_layer_identity() {
    let a = tagged(1, 7, 96, 54);
    assert_parity(&[(&a, 1.0)], 96, 54, Fit::Fill, "identity tagged");
    let n = noise(64, 48, 1);
    assert_parity(&[(&n, 1.0)], 64, 48, Fit::Fill, "identity noise");
}

#[test]
fn identity_is_exact() {
    let Some(ctx) = ctx() else { return };
    let a = noise(37, 23, 9);
    let got = gpu(&ctx, &[(&a, 1.0)], 37, 23, Fit::Fill);
    assert_eq!(got.data, a.data);
}

#[test]
fn four_by_three_into_sixteen_by_nine_fill() {
    // Upscale and crop (typical without decoder pre-scaling)...
    let a = tagged(2, 3, 160, 120);
    assert_parity(&[(&a, 1.0)], 320, 180, Fit::Fill, "4:3 fill up");
    let n = noise(160, 120, 2);
    assert_parity(&[(&n, 1.0)], 320, 180, Fit::Fill, "4:3 fill up noise");
    // ...mild downscale, and a decoder pre-scaled source (crop only).
    let n = noise(400, 300, 3);
    assert_parity(&[(&n, 1.0)], 320, 180, Fit::Fill, "4:3 fill down noise");
    let n = noise(320, 240, 4);
    assert_parity(&[(&n, 1.0)], 320, 180, Fit::Fill, "4:3 fill crop noise");
}

#[test]
fn fit_letterbox_and_pillarbox() {
    // 16:9 into 4:3: bars top and bottom.
    let a = tagged(3, 1, 320, 180);
    assert_parity(&[(&a, 1.0)], 240, 180, Fit::Fit, "letterbox tagged");
    let n = noise(160, 90, 5);
    assert_parity(&[(&n, 1.0)], 240, 180, Fit::Fit, "letterbox noise");
    // 4:3 into 16:9: bars left and right.
    let n = noise(120, 90, 6);
    assert_parity(&[(&n, 1.0)], 320, 180, Fit::Fit, "pillarbox noise");

    // The bars are opaque black.
    let Some(ctx) = ctx() else { return };
    let a = VideoFrame::solid(Micros(0), 160, 90, [200, 100, 50, 255]);
    let got = gpu(&ctx, &[(&a, 1.0)], 240, 180, Fit::Fit);
    let px = |x: usize, y: usize| &got.data[(y * 240 + x) * 4..][..4];
    assert_eq!(px(120, 0), [0, 0, 0, 255]);
    assert_eq!(px(120, 179), [0, 0, 0, 255]);
    assert_eq!(px(120, 90), [200, 100, 50, 255]);
}

#[test]
fn crossfade_fifty_fifty() {
    let a = tagged(4, 10, 192, 108);
    let b = tagged(5, 20, 192, 108);
    assert_parity(&[(&a, 0.5), (&b, 0.5)], 192, 108, Fit::Fill, "crossfade");
    // Mixed sizes and aspect ratios, noise.
    let a = noise(192, 108, 7);
    let b = noise(120, 90, 8);
    assert_parity(
        &[(&a, 0.5), (&b, 0.5)],
        192,
        108,
        Fit::Fill,
        "crossfade fill mixed",
    );
    assert_parity(
        &[(&a, 0.3), (&b, 0.7)],
        192,
        108,
        Fit::Fit,
        "crossfade fit mixed",
    );
}

#[test]
fn odd_sizes_and_both_fits() {
    let sizes = [(17, 31), (64, 64), (101, 57), (33, 90)];
    let outs = [(48, 27), (27, 48), (50, 50)];
    for (i, &(sw, sh)) in sizes.iter().enumerate() {
        let n = noise(sw, sh, 100 + i as u32);
        for &(w, h) in &outs {
            for fit in [Fit::Fill, Fit::Fit] {
                let what = format!("{sw}x{sh} -> {w}x{h} {fit:?}");
                assert_parity(&[(&n, 1.0)], w, h, fit, &what);
            }
        }
    }
}

#[test]
fn no_layers_is_opaque_black() {
    let Some(ctx) = ctx() else { return };
    let mut c = GpuCompositor::new(&ctx, 8, 8).unwrap();
    c.render(&[], Fit::Fill).unwrap();
    let got = c.read_back().unwrap();
    assert_eq!(got.data, cpu(&[], 8, 8, Fit::Fill).data);
    assert!(got.data.chunks_exact(4).all(|p| p == [0, 0, 0, 255]));
}

#[test]
fn textures_are_pooled_by_size() {
    let Some(ctx) = ctx() else { return };
    let mut c = GpuCompositor::new(&ctx, 32, 18).unwrap();
    let a = noise(32, 18, 1);
    let b = noise(32, 18, 2);
    let small = noise(16, 16, 3);

    let ia = c.upload(&a).unwrap();
    assert_eq!(c.texture_count(), 1);
    c.release(ia).unwrap();
    // Same size: the released texture is reused, with the new contents.
    let ib = c.upload(&b).unwrap();
    assert_ne!(ia, ib);
    assert_eq!(c.texture_count(), 1);
    c.render(&[(ib, 1.0)], Fit::Fill).unwrap();
    assert_eq!(c.read_back().unwrap().data, b.data);
    // Another size, or a size whose textures are all in use: a new one.
    let is = c.upload(&small).unwrap();
    assert_eq!(c.texture_count(), 2);
    let ia2 = c.upload(&a).unwrap();
    assert_eq!(c.texture_count(), 3);
    c.release(is).unwrap();
    c.release(ia2).unwrap();
    c.release(ib).unwrap();
    assert_eq!(c.texture_count(), 3);
    let _ = c.upload(&small).unwrap();
    let _ = c.upload(&a).unwrap();
    assert_eq!(c.texture_count(), 3);
}

#[test]
fn bad_ids_and_frames_are_errors() {
    let Some(ctx) = ctx() else { return };
    let mut c = GpuCompositor::new(&ctx, 8, 8).unwrap();
    let id = c.upload(&noise(8, 8, 1)).unwrap();
    c.release(id).unwrap();
    assert_eq!(c.release(id), Err(GpuError::UnknownTexture(id)));
    assert_eq!(
        c.render(&[(id, 1.0)], Fit::Fill),
        Err(GpuError::UnknownTexture(id))
    );
    let mut short = noise(8, 8, 2);
    short.data.pop();
    assert!(matches!(
        c.upload(&short),
        Err(GpuError::InvalidSize { which: "frame", .. })
    ));
    assert!(matches!(
        GpuCompositor::new(&ctx, 0, 8),
        Err(GpuError::InvalidSize { .. })
    ));
}

#[test]
fn timing_1080p_two_layer_render_and_readback() {
    let Some(ctx) = ctx() else { return };
    let a = noise(1920, 1080, 1);
    let b = noise(1920, 1080, 2);
    let c43 = noise(1440, 1080, 3);
    let mut c = GpuCompositor::new(&ctx, 1920, 1080).unwrap();
    for (name, second) in [("pre-scaled", &b), ("4:3 fill", &c43)] {
        let runs = 5u32;
        // Warm-up (pipeline, first allocation).
        let ia = c.upload(&a).unwrap();
        let ib = c.upload(second).unwrap();
        c.render(&[(ia, 0.5), (ib, 0.5)], Fit::Fill).unwrap();
        c.read_back().unwrap();
        c.release(ia).unwrap();
        c.release(ib).unwrap();

        let (mut upload, mut render) = (0.0, 0.0);
        for _ in 0..runs {
            let t = std::time::Instant::now();
            let ia = c.upload(&a).unwrap();
            let ib = c.upload(second).unwrap();
            let t1 = std::time::Instant::now();
            c.render(&[(ia, 0.5), (ib, 0.5)], Fit::Fill).unwrap();
            c.read_back().unwrap();
            render += t1.elapsed().as_secs_f64();
            upload += (t1 - t).as_secs_f64();
            c.release(ia).unwrap();
            c.release(ib).unwrap();
        }
        let ms = |s: f64| s * 1000.0 / f64::from(runs);
        println!(
            "gpu 1080p two-layer ({name}): upload {:.2} ms, render + readback {:.2} ms per frame",
            ms(upload),
            ms(render)
        );
    }
}

/// The title card overlay (#56): a rasterised title alpha-blended over a
/// 4:3-into-16:9 crossfade (and over black) matches `kv_render::overlay`.
#[test]
fn title_overlay_matches_cpu() {
    let Some(ctx) = ctx() else { return };
    let (w, h) = (320, 180);
    let title = kv_render::title::rasterize(
        &kv_core::TitleCard {
            text: "Summer 2026".into(),
            duration: Micros(2_000_000),
        },
        w,
        h,
    );
    let a = tagged(2, 3, 160, 120);
    let b = noise(w, h, 9);
    for (layers, opacity, what) in [
        (vec![(&a, 0.5), (&b, 0.5)], 1.0, "crossfade, opaque title"),
        (vec![(&a, 1.0)], 0.4, "fill, fading title"),
        (vec![], 0.8, "black, title"),
    ] {
        let mut want = cpu(&layers, w, h, Fit::Fill);
        kv_render::overlay(&mut want, &title, opacity).unwrap();

        let mut c = GpuCompositor::new(&ctx, w, h).unwrap();
        let ids: Vec<_> = layers
            .iter()
            .map(|&(f, weight)| (c.upload(f).unwrap(), weight))
            .collect();
        let t = c.upload(&title).unwrap();
        c.render_with_overlays(&ids, &[(t, opacity)], Fit::Fill)
            .unwrap();
        let got = c.read_back().unwrap();

        // The 4:3 layer is cropped top and bottom only (full width): skip
        // the first and last row (crop edges), compare everything else.
        let mut max = 0u8;
        for y in 1..h as usize - 1 {
            for x in 0..w as usize {
                let i = (y * w as usize + x) * 4;
                for ch in 0..4 {
                    let d = got.data[i + ch].abs_diff(want.data[i + ch]);
                    assert!(
                        d <= TOLERANCE,
                        "{what}: ({x}, {y}) channel {ch}: gpu {} cpu {}",
                        got.data[i + ch],
                        want.data[i + ch]
                    );
                    max = max.max(d);
                }
            }
        }
        println!("{what}: max channel difference {max}");
        // The title is really there: many pixels differ from no overlay.
        let plain = cpu(&layers, w, h, Fit::Fill);
        let changed = got
            .data
            .chunks_exact(4)
            .zip(plain.data.chunks_exact(4))
            .filter(|(g, p)| g[0].abs_diff(p[0]) > 20)
            .count();
        println!("{what}: {changed} pixels show the title");
        assert!(
            changed > 150,
            "{what}: only {changed} pixels show the title"
        );
    }
}

#[test]
fn overlay_of_the_wrong_size_is_rejected() {
    let Some(ctx) = ctx() else { return };
    let mut c = GpuCompositor::new(&ctx, 8, 8).unwrap();
    let small = c
        .upload(&VideoFrame::solid(Micros(0), 4, 4, [255; 4]))
        .unwrap();
    assert!(matches!(
        c.render_with_overlays(&[], &[(small, 1.0)], Fit::Fill),
        Err(GpuError::InvalidSize {
            which: "overlay",
            ..
        })
    ));
}
