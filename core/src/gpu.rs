pub struct Pipelines {
    // Preview
    pub grade_pipeline: wgpu::ComputePipeline,
    pub grade_bgl: wgpu::BindGroupLayout,
    #[allow(dead_code)]
    pub out_texture: wgpu::Texture,
    pub out_view: wgpu::TextureView,

    // Export Y
    pub grade_y_pipeline: wgpu::ComputePipeline,
    pub grade_y_bgl: wgpu::BindGroupLayout,
    pub out_y_texture: wgpu::Texture,
    pub out_y_view: wgpu::TextureView,

    // Export UV
    pub grade_uv_pipeline: wgpu::ComputePipeline,
    pub grade_uv_bgl: wgpu::BindGroupLayout,
    pub out_uv_texture: wgpu::Texture,
    pub out_uv_view: wgpu::TextureView,

    // Preview blit (bind group is built per-frame in app.rs)
    pub blit_pipeline: wgpu::RenderPipeline,
    pub blit_bgl: wgpu::BindGroupLayout,
    pub blit_sampler: wgpu::Sampler,

    // Diagnostic: N chained no-op effect passes
    pub noop_pipeline: wgpu::ComputePipeline,
    pub noop_bgl: wgpu::BindGroupLayout,
    #[allow(dead_code)]
    pub ping_a_texture: wgpu::Texture,
    pub ping_a_view: wgpu::TextureView,
    #[allow(dead_code)]
    pub ping_b_texture: wgpu::Texture,
    pub ping_b_view: wgpu::TextureView,
}

fn make_yuv_grade_bgl(
    device: &wgpu::Device,
    storage_format: wgpu::TextureFormat,
    label: &str,
) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: storage_format,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            },
        ],
    })
}

fn make_grade_pipeline(
    device: &wgpu::Device,
    bgl: &wgpu::BindGroupLayout,
    shader: &wgpu::ShaderModule,
    label: &str,
) -> wgpu::ComputePipeline {
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(bgl)],
        immediate_size: 0,
    });
    device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&pl),
        module: shader,
        entry_point: Some("main"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    })
}

fn make_noop_bgl(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("noop-bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            },
        ],
    })
}

fn make_ping_texture(device: &wgpu::Device, label: &str, vw: u32, vh: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: vw,
            height: vh,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

pub fn build_pipelines(
    host: &grafting::HostWgpuContext,
    surface_format: wgpu::TextureFormat,
    vw: u32,
    vh: u32,
) -> Pipelines {
    let uv_w = vw / 2;
    let uv_h = vh / 2;
    let device = &host.device;

    // Preview RGBA output (the grade target).
    let out_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("graded-rgba"),
        size: wgpu::Extent3d {
            width: vw,
            height: vh,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let out_view = out_texture.create_view(&wgpu::TextureViewDescriptor::default());

    // Ping-pong intermediates for the diagnostic effect chain.
    let ping_a_texture = make_ping_texture(device, "ping-a", vw, vh);
    let ping_a_view = ping_a_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let ping_b_texture = make_ping_texture(device, "ping-b", vw, vh);
    let ping_b_view = ping_b_texture.create_view(&wgpu::TextureViewDescriptor::default());

    // Export Y (packed R32Uint).
    let y_tex_w = vw / 4;
    let out_y_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("graded-y-packed"),
        size: wgpu::Extent3d {
            width: y_tex_w,
            height: vh,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Uint,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let out_y_view = out_y_texture.create_view(&wgpu::TextureViewDescriptor::default());

    // Export UV (packed R32Uint).
    let uv_tex_w = uv_w / 2;
    let out_uv_texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("graded-uv-packed"),
        size: wgpu::Extent3d {
            width: uv_tex_w,
            height: uv_h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Uint,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let out_uv_view = out_uv_texture.create_view(&wgpu::TextureViewDescriptor::default());

    // Shaders
    let grade_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("grade-shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("grade.wgsl").into()),
    });
    let grade_y_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("grade-y-shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("grade_y.wgsl").into()),
    });
    let grade_uv_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("grade-uv-shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("grade_uv.wgsl").into()),
    });
    let noop_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("noop-shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("noop.wgsl").into()),
    });

    let grade_bgl = make_yuv_grade_bgl(device, wgpu::TextureFormat::Rgba8Unorm, "grade-bgl");
    let grade_y_bgl = make_yuv_grade_bgl(device, wgpu::TextureFormat::R32Uint, "grade-y-bgl");
    let grade_uv_bgl = make_yuv_grade_bgl(device, wgpu::TextureFormat::R32Uint, "grade-uv-bgl");
    let noop_bgl = make_noop_bgl(device);

    let grade_pipeline = make_grade_pipeline(device, &grade_bgl, &grade_shader, "grade-pipeline");
    let grade_y_pipeline =
        make_grade_pipeline(device, &grade_y_bgl, &grade_y_shader, "grade-y-pipeline");
    let grade_uv_pipeline =
        make_grade_pipeline(device, &grade_uv_bgl, &grade_uv_shader, "grade-uv-pipeline");
    let noop_pipeline = make_grade_pipeline(device, &noop_bgl, &noop_shader, "noop-pipeline");

    // Blit pipeline
    let blit_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("blit-shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("blit.wgsl").into()),
    });
    let blit_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("blit-bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
        ],
    });
    let blit_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("blit-pl"),
        bind_group_layouts: &[Some(&blit_bgl)],
        immediate_size: 0,
    });
    let blit_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("blit-pipeline"),
        layout: Some(&blit_pl),
        vertex: wgpu::VertexState {
            module: &blit_shader,
            entry_point: Some("vs"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &blit_shader,
            entry_point: Some("fs"),
            targets: &[Some(wgpu::ColorTargetState {
                format: surface_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });

    let blit_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("blit-sampler"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    Pipelines {
        grade_pipeline,
        grade_bgl,
        out_texture,
        out_view,
        grade_y_pipeline,
        grade_y_bgl,
        out_y_texture,
        out_y_view,
        grade_uv_pipeline,
        grade_uv_bgl,
        out_uv_texture,
        out_uv_view,
        blit_pipeline,
        blit_bgl,
        blit_sampler,
        noop_pipeline,
        noop_bgl,
        ping_a_texture,
        ping_a_view,
        ping_b_texture,
        ping_b_view,
    }
}

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
pub struct CacheKey {
    pub dev: u64,
    pub ino: u64,
    pub offset: u64,
    pub modifier: u64,
}

pub struct CachedNv12 {
    #[allow(dead_code)]
    pub y_texture: wgpu::Texture,
    #[allow(dead_code)]
    pub uv_texture: wgpu::Texture,
    pub y_view: wgpu::TextureView,
    pub uv_view: wgpu::TextureView,
}
