use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use wgpu::util::DeviceExt;
use wgpu::*;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FRAME_BYTES: usize = (WIDTH * HEIGHT * 4) as usize;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    resolution: [u32; 2],
    gamma: f32,
    saturation: f32,
}

fn main() {
    env_logger::init();

    let input = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/h264_test.mp4".to_string());
    println!("Input: {}", input);

    // ---- 1. Spawn FFmpeg: VA-API decode → RGBA on stdout ----
    let mut child = Command::new("ffmpeg")
        .args(&[
            "-hide_banner",
            "-loglevel",
            "error",
            "-hwaccel",
            "vaapi",
            "-hwaccel_device",
            "/dev/dri/renderD128",
            "-hwaccel_output_format",
            "vaapi",
            "-i",
            &input,
            "-vf",
            "hwdownload,format=rgba",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to spawn ffmpeg");
    let mut stdout = child.stdout.take().unwrap();

    // ---- 2. wgpu setup (same pipeline as before) ----
    let instance = Instance::new(&InstanceDescriptor {
        backends: Backends::VULKAN,
        ..Default::default()
    });
    let adapter = pollster::block_on(instance.request_adapter(&RequestAdapterOptions {
        power_preference: PowerPreference::LowPower,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .expect("no adapter");
    let (device, queue) = pollster::block_on(adapter.request_device(
        &DeviceDescriptor {
            label: Some("decode-grade"),
            required_features: Features::empty(),
            required_limits: Limits::default(),
            memory_hints: MemoryHints::default(),
        },
        None,
    ))
    .expect("device");

    let input_tex = device.create_texture(&TextureDescriptor {
        label: Some("input"),
        size: Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8Unorm,
        usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let output_tex = device.create_texture(&TextureDescriptor {
        label: Some("output"),
        size: Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8Unorm,
        usage: TextureUsages::STORAGE_BINDING | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let input_view = input_tex.create_view(&TextureViewDescriptor::default());
    let output_view = output_tex.create_view(&TextureViewDescriptor::default());

    let params = Params {
        resolution: [WIDTH, HEIGHT],
        gamma: 1.2,
        saturation: 0.0,
    };
    let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("params"),
        contents: bytemuck::bytes_of(&params),
        usage: BufferUsages::UNIFORM,
    });

    let shader = device.create_shader_module(ShaderModuleDescriptor {
        label: Some("grade"),
        source: ShaderSource::Wgsl(include_str!("grade.wgsl").into()),
    });

    let bgl = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
        label: None,
        entries: &[
            BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::COMPUTE,
                ty: BindingType::Texture {
                    sample_type: TextureSampleType::Float { filterable: true },
                    view_dimension: TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            BindGroupLayoutEntry {
                binding: 1,
                visibility: ShaderStages::COMPUTE,
                ty: BindingType::StorageTexture {
                    access: StorageTextureAccess::WriteOnly,
                    format: TextureFormat::Rgba8Unorm,
                    view_dimension: TextureViewDimension::D2,
                },
                count: None,
            },
            BindGroupLayoutEntry {
                binding: 2,
                visibility: ShaderStages::COMPUTE,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });

    let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[&bgl],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_compute_pipeline(&ComputePipelineDescriptor {
        label: Some("grade-pipeline"),
        layout: Some(&pipeline_layout),
        module: &shader,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });
    let bind_group = device.create_bind_group(&BindGroupDescriptor {
        label: None,
        layout: &bgl,
        entries: &[
            BindGroupEntry {
                binding: 0,
                resource: BindingResource::TextureView(&input_view),
            },
            BindGroupEntry {
                binding: 1,
                resource: BindingResource::TextureView(&output_view),
            },
            BindGroupEntry {
                binding: 2,
                resource: params_buf.as_entire_binding(),
            },
        ],
    });

    let readback = device.create_buffer(&BufferDescriptor {
        label: Some("readback"),
        size: FRAME_BYTES as u64,
        usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let wx = (WIDTH + 7) / 8;
    let wy = (HEIGHT + 7) / 8;

    // ---- 3. Stream frames ----
    let mut frame_buf = vec![0u8; FRAME_BYTES];
    let mut frame_idx: u64 = 0;
    let mut decode_time = Duration::ZERO;
    let mut upload_time = Duration::ZERO;
    let mut gpu_time = Duration::ZERO;
    let mut saved_png = false;

    let total_start = Instant::now();

    loop {
        // Decode one frame from ffmpeg pipe
        let t = Instant::now();
        if stdout.read_exact(&mut frame_buf).is_err() {
            break;
        }
        decode_time += t.elapsed();

        // Upload to GPU
        let t = Instant::now();
        queue.write_texture(
            TexelCopyTextureInfo {
                texture: &input_tex,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            &frame_buf,
            TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(WIDTH * 4),
                rows_per_image: Some(HEIGHT),
            },
            Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        upload_time += t.elapsed();

        // Dispatch + copy to readback buffer
        let t = Instant::now();
        let mut enc = device.create_command_encoder(&CommandEncoderDescriptor::default());
        {
            let mut cp = enc.begin_compute_pass(&ComputePassDescriptor {
                label: Some("grade"),
                timestamp_writes: None,
            });
            cp.set_pipeline(&pipeline);
            cp.set_bind_group(0, &bind_group, &[]);
            cp.dispatch_workgroups(wx, wy, 1);
        }
        enc.copy_texture_to_buffer(
            TexelCopyTextureInfo {
                texture: &output_tex,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            TexelCopyBufferInfo {
                buffer: &readback,
                layout: TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(WIDTH * 4),
                    rows_per_image: Some(HEIGHT),
                },
            },
            Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([enc.finish()]);
        device.poll(Maintain::Wait);
        gpu_time += t.elapsed();

        // Save one middle frame for visual verification
        if !saved_png && frame_idx == 100 {
            let slice = readback.slice(..);
            let (tx, rx) = mpsc::channel();
            slice.map_async(MapMode::Read, move |r| tx.send(r).unwrap());
            device.poll(Maintain::Wait);
            rx.recv().unwrap().expect("map");
            {
                let data = slice.get_mapped_range();
                let img = image::RgbaImage::from_raw(WIDTH, HEIGHT, data.to_vec()).unwrap();
                img.save("/tmp/decoded_grade_frame.png").unwrap();
            }
            readback.unmap();
            saved_png = true;
            println!("saved /tmp/decoded_grade_frame.png (frame {})", frame_idx);
        }

        frame_idx += 1;
    }

    let total = total_start.elapsed();
    let n = frame_idx.max(1) as f64;

    println!();
    println!(
        "=== {} frames in {:?} ({:.1} fps) ===",
        frame_idx,
        total,
        frame_idx as f64 / total.as_secs_f64()
    );
    println!(
        "avg decode  (ffmpeg pipe): {:>7.2} ms",
        decode_time.as_secs_f64() * 1000.0 / n
    );
    println!(
        "avg upload  (write_tex)  : {:>7.2} ms",
        upload_time.as_secs_f64() * 1000.0 / n
    );
    println!(
        "avg gpu     (dispatch+cp): {:>7.2} ms",
        gpu_time.as_secs_f64() * 1000.0 / n
    );

    let _ = child.wait();
}
