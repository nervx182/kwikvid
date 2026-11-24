// One weighted layer of the GPU compositor (kv_render::gpu). Drawn once per
// layer into the same render pass with additive blending (One + One).
//
// Mirrors the CPU compositor (composite.rs, ADR 0009): bilinear resampling
// with pixel-centre alignment and edge clamping, in gamma-encoded values,
// and uncovered output pixels count as opaque black for the layer. The
// taps are computed and loaded by hand (textureLoad) rather than through a
// hardware sampler, whose sub-texel precision varies by GPU.

struct Layer {
    // Covered output rectangle: x0, x1, y0, y1 (half open).
    rect: vec4<i32>,
    // Top-left of the scaled source in output pixels (may be negative).
    offset: vec2<i32>,
    // Source pixels per scaled pixel, per axis.
    ratio: vec2<f32>,
    weight: f32,
}

@group(0) @binding(0) var<uniform> layer: Layer;
@group(0) @binding(1) var src: texture_2d<f32>;

// Full-screen triangle.
@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let x = f32(i32(i & 1u) * 4 - 1);
    let y = f32(i32(i >> 1u) * 4 - 1);
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let p = vec2<i32>(floor(pos.xy));
    let r = layer.rect;
    if (p.x < r.x || p.x >= r.y || p.y < r.z || p.y >= r.w) {
        return vec4<f32>(0.0, 0.0, 0.0, layer.weight);
    }
    let dims = vec2<i32>(textureDimensions(src));
    let max_s = vec2<f32>(dims - vec2<i32>(1, 1));
    let s = clamp((vec2<f32>(p - layer.offset) + 0.5) * layer.ratio - 0.5, vec2<f32>(0.0), max_s);
    let i0 = vec2<i32>(floor(s));
    let i1 = min(i0 + vec2<i32>(1, 1), dims - vec2<i32>(1, 1));
    let t = s - vec2<f32>(i0);
    let c00 = textureLoad(src, vec2<i32>(i0.x, i0.y), 0);
    let c10 = textureLoad(src, vec2<i32>(i1.x, i0.y), 0);
    let c01 = textureLoad(src, vec2<i32>(i0.x, i1.y), 0);
    let c11 = textureLoad(src, vec2<i32>(i1.x, i1.y), 0);
    let top = c00 * (1.0 - t.x) + c10 * t.x;
    let bottom = c01 * (1.0 - t.x) + c11 * t.x;
    return (top * (1.0 - t.y) + bottom * t.y) * layer.weight;
}
