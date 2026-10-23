//! Offscreen wgpu renderer with a small HDR pipeline:
//!
//! 1. shadow map from the sun,
//! 2. scene pass (MSAA) writing direct light, ambient light and view-space normal/depth,
//! 3. screen-space ambient occlusion and a depth-aware blur,
//! 4. composite: direct + ambient * AO, ACES tone mapping, vignette and grading.
//!
//! Frames are read back as BGRA bytes so the GPUI window can show them as an image.

use crate::mesh::Vertex;
use crate::scene::{Material, Scene};
use anyhow::{Context, Result};
use glam::{Mat4, Vec2, Vec3, Vec4};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    thread,
};
use wgpu::util::DeviceExt;

const SAMPLES: u32 = 4;
const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const AO_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;
const OUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;
const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
const SHADOW_SIZE: u32 = 4096;
const INSTANCE_STRIDE: u64 = 256;
const MAX_LIGHTS: usize = 4;
const FOV_Y: f32 = 40.0;

/// What the viewport shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum View {
    /// The house, yard and street.
    #[default]
    World,
    /// Only the robot, on a photo-studio cyclorama with studio lighting.
    Studio { dark: bool },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub target: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
}

impl Camera {
    pub fn eye(&self) -> Vec3 {
        let dir = Vec3::new(
            self.pitch.cos() * self.yaw.cos(),
            self.pitch.cos() * self.yaw.sin(),
            self.pitch.sin(),
        );
        self.target + dir * self.distance
    }

    fn view(&self) -> Mat4 {
        glam::camera::rh::view::look_at_mat4(self.eye(), self.target, Vec3::Z)
    }

    fn proj(aspect: f32) -> Mat4 {
        glam::camera::rh::proj::directx::perspective(FOV_Y.to_radians(), aspect, 0.05, 160.0)
    }

    /// Moves the orbit target in the camera's screen plane.
    pub fn pan(&mut self, dx: f32, dy: f32) {
        let forward = (self.target - self.eye()).normalize();
        let right = forward.cross(Vec3::Z).normalize();
        let up = right.cross(forward);
        let scale = self.distance * 0.0015;
        self.target += (-right * dx + up * dy) * scale;
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
    inv_view_proj: [[f32; 4]; 4],
    view: [[f32; 4]; 4],
    light_view_proj: [[f32; 4]; 4],
    camera_pos: [f32; 4],
    /// xyz: direction the sunlight travels, w: intensity
    sun_dir: [f32; 4],
    sun_color: [f32; 4],
    /// rgb: color, a: ambient strength
    sky: [f32; 4],
    ground: [f32; 4],
    /// xyz: world position, w: range
    light_pos: [[f32; 4]; MAX_LIGHTS],
    /// rgb: color * intensity
    light_color: [[f32; 4]; MAX_LIGHTS],
    /// x: light count, y: width, z: height, w: tan(fov_y / 2)
    params: [f32; 4],
    /// rgb: fog colour, a: distance where fog starts
    fog: [f32; 4],
    /// x: fog falloff distance, y: 1 in the studio (backdrop colour instead of the sky)
    fog_params: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    model: [[f32; 4]; 4],
    normal: [[f32; 4]; 4],
    base_color: [f32; 4],
    params: [f32; 4],
    emissive: [f32; 4],
    uv_scale: [f32; 4],
}

struct GpuMesh {
    vertices: wgpu::Buffer,
    indices: wgpu::Buffer,
    count: u32,
    /// Bounding-box centre in the mesh's own frame, for the wall cutaway.
    center: Vec3,
}

struct Targets {
    width: u32,
    height: u32,
    msaa: [wgpu::TextureView; 3],
    depth: wgpu::TextureView,
    resolved: [wgpu::TextureView; 3],
    ao_raw: wgpu::TextureView,
    ao: wgpu::TextureView,
    out: wgpu::Texture,
    out_view: wgpu::TextureView,
    ssao_group: wgpu::BindGroup,
    blur_group: wgpu::BindGroup,
    composite_group: wgpu::BindGroup,
    readback: wgpu::Buffer,
    padded_row: u32,
}

/// Downloads textures in the background and caches them on disk.
#[derive(Clone, Default)]
struct TextureLoader {
    done: Arc<Mutex<HashMap<String, Option<Arc<image::RgbaImage>>>>>,
    /// Folder of the world file; relative texture URLs resolve against it.
    base_dir: std::path::PathBuf,
}

impl TextureLoader {
    fn request(&self, url: &str) {
        let mut done = self.done.lock().unwrap();
        if done.contains_key(url) {
            return;
        }
        done.insert(url.to_string(), None);
        let (url, done, base_dir) = (url.to_string(), self.done.clone(), self.base_dir.clone());
        thread::spawn(move || {
            if let Ok(img) = fetch_texture(&url, &base_dir) {
                done.lock().unwrap().insert(url, Some(Arc::new(img)));
            }
        });
    }

    /// Images stay cached because several materials can share one texture.
    fn ready(&self, url: &str) -> Option<Arc<image::RgbaImage>> {
        self.done.lock().unwrap().get(url).cloned().flatten()
    }
}

fn fetch_texture(url: &str, base_dir: &std::path::Path) -> Result<image::RgbaImage> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Ok(image::open(base_dir.join(url))?.to_rgba8());
    }
    // The app bundle seeds Webots' asset cache (files named by the SHA-1 of their URL), which
    // holds every texture the world uses, so nothing has to be downloaded.
    let webots_cache = std::path::Path::new(&std::env::var("HOME").unwrap_or_default())
        .join("Library/Caches/Cyberbotics/Webots/assets")
        .join(sha1_smol::Sha1::from(url).digest().to_string());
    let cache = dirs_cache().join(format!("{:016x}", fxhash(url)));
    let bytes = match std::fs::read(&webots_cache).or_else(|_| std::fs::read(&cache)) {
        Ok(b) => b,
        Err(_) => {
            let b = ureq::get(url).call()?.body_mut().read_to_vec()?;
            let _ = std::fs::create_dir_all(cache.parent().unwrap());
            let _ = std::fs::write(&cache, &b);
            b
        }
    };
    let img = image::load_from_memory(&bytes)?.to_rgba8();
    // Big textures only cost memory here; 1024 is plenty for a viewer.
    Ok(if img.width() > 1024 || img.height() > 1024 {
        image::imageops::thumbnail(&img, img.width().min(1024), img.height().min(1024))
    } else {
        img
    })
}

fn dirs_cache() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::Path::new(&home).join("Library/Caches/nao-eph-sim/textures")
}

fn fxhash(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

fn uniform_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    dynamic: bool,
    size: Option<u64>,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: dynamic,
            min_binding_size: size.and_then(wgpu::BufferSize::new),
        },
        count: None,
    }
}

fn texture_entry(binding: u32, sample_type: wgpu::TextureSampleType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

const UNFILTERED: wgpu::TextureSampleType = wgpu::TextureSampleType::Float { filterable: false };

pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    opaque_pipeline: wgpu::RenderPipeline,
    transparent_pipeline: wgpu::RenderPipeline,
    shadow_pipeline: wgpu::RenderPipeline,
    ssao_pipeline: wgpu::RenderPipeline,
    blur_pipeline: wgpu::RenderPipeline,
    composite_pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    globals_group: wgpu::BindGroup,
    shadow_globals_group: wgpu::BindGroup,
    instance_layout: wgpu::BindGroupLayout,
    instances: wgpu::Buffer,
    instance_capacity: u64,
    instance_group: wgpu::BindGroup,
    texture_layout: wgpu::BindGroupLayout,
    post_layout: wgpu::BindGroupLayout,
    sky_layout: wgpu::BindGroupLayout,
    sky_group: wgpu::BindGroup,
    sky_loaded: bool,
    sampler: wgpu::Sampler,
    white: wgpu::BindGroup,
    shadow_view: wgpu::TextureView,
    backdrop: GpuMesh,
    targets: Option<Targets>,
    // Per loaded scene.
    generation: u64,
    meshes: Vec<GpuMesh>,
    material_groups: Vec<Option<wgpu::BindGroup>>,
    loader: TextureLoader,
}

impl Renderer {
    pub fn new(world_dir: std::path::PathBuf) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .context("no GPU adapter")?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("nao-eph-sim"),
            ..Default::default()
        }))?;

        let scene_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scene"),
            source: wgpu::ShaderSource::Wgsl(include_str!("scene.wgsl").into()),
        });
        let post_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("post"),
            source: wgpu::ShaderSource::Wgsl(include_str!("post.wgsl").into()),
        });

        let all = wgpu::ShaderStages::VERTEX_FRAGMENT;
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[
                uniform_entry(0, all, false, None),
                texture_entry(1, wgpu::TextureSampleType::Depth),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                    count: None,
                },
            ],
        });
        // The shadow pass writes the shadow map, so it must not also bind it for reading.
        let shadow_globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("shadow globals"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::VERTEX, false, None)],
        });
        let instance_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("instance"),
            entries: &[uniform_entry(0, all, true, Some(std::mem::size_of::<Instance>() as u64))],
        });
        let texture_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("material"),
            entries: &[
                texture_entry(0, wgpu::TextureSampleType::Float { filterable: true }),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        // Post passes read up to three full-screen textures with textureLoad, plus the globals.
        let post_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("post"),
            entries: &[
                uniform_entry(0, wgpu::ShaderStages::FRAGMENT, false, None),
                texture_entry(1, UNFILTERED),
                texture_entry(2, UNFILTERED),
                texture_entry(3, UNFILTERED),
                texture_entry(4, UNFILTERED),
            ],
        });

        let sky_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sky"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::Cube,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let scene_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scene"),
            bind_group_layouts: &[Some(&globals_layout), Some(&instance_layout), Some(&texture_layout)],
            immediate_size: 0,
        });
        let shadow_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shadow"),
            bind_group_layouts: &[Some(&shadow_globals_layout), Some(&instance_layout), Some(&texture_layout)],
            immediate_size: 0,
        });
        let post_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("post"),
            bind_group_layouts: &[Some(&post_layout), Some(&sky_layout)],
            immediate_size: 0,
        });
        let vertex_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2],
        };
        let depth_state = |bias| {
            Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: Default::default(),
                bias,
            })
        };
        let scene_pipeline = |label, blend: Option<wgpu::BlendState>, normals: wgpu::ColorWrites| {
            let hdr = Some(wgpu::ColorTargetState { format: HDR_FORMAT, blend, write_mask: wgpu::ColorWrites::ALL });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&scene_layout),
                vertex: wgpu::VertexState {
                    module: &scene_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[Some(vertex_layout.clone())],
                },
                primitive: wgpu::PrimitiveState { cull_mode: None, ..Default::default() },
                depth_stencil: depth_state(Default::default()),
                multisample: wgpu::MultisampleState { count: SAMPLES, ..Default::default() },
                fragment: Some(wgpu::FragmentState {
                    module: &scene_shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[
                        hdr.clone(),
                        hdr,
                        Some(wgpu::ColorTargetState { format: HDR_FORMAT, blend: None, write_mask: normals }),
                    ],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let opaque_pipeline = scene_pipeline("opaque", None, wgpu::ColorWrites::ALL);
        // Glass blends over what is behind it but keeps that surface's normal and depth for SSAO.
        let transparent_pipeline =
            scene_pipeline("transparent", Some(wgpu::BlendState::ALPHA_BLENDING), wgpu::ColorWrites::empty());
        let shadow_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("shadow"),
            layout: Some(&shadow_layout),
            vertex: wgpu::VertexState {
                module: &scene_shader,
                entry_point: Some("vs_shadow"),
                compilation_options: Default::default(),
                buffers: &[Some(vertex_layout)],
            },
            primitive: wgpu::PrimitiveState { cull_mode: None, ..Default::default() },
            depth_stencil: depth_state(wgpu::DepthBiasState { constant: 2, slope_scale: 2.0, clamp: 0.0 }),
            multisample: Default::default(),
            // Only discards transparent texels (tree leaves); depth is written by the rasterizer.
            fragment: Some(wgpu::FragmentState {
                module: &scene_shader,
                entry_point: Some("fs_shadow"),
                compilation_options: Default::default(),
                targets: &[],
            }),
            multiview_mask: None,
            cache: None,
        });
        let post_pipeline = |label, entry, format| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&post_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &post_shader,
                    entry_point: Some("vs_fullscreen"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &post_shader,
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        };
        let ssao_pipeline = post_pipeline("ssao", "fs_ssao", AO_FORMAT);
        let blur_pipeline = post_pipeline("blur", "fs_blur", AO_FORMAT);
        let composite_pipeline = post_pipeline("composite", "fs_composite", OUT_FORMAT);

        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let shadow_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shadow map"),
            size: wgpu::Extent3d { width: SHADOW_SIZE, height: SHADOW_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let shadow_view = shadow_texture.create_view(&Default::default());
        let shadow_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shadow"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            compare: Some(wgpu::CompareFunction::LessEqual),
            ..Default::default()
        });
        let globals_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &globals_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&shadow_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&shadow_sampler) },
            ],
        });
        let shadow_globals_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("shadow globals"),
            layout: &shadow_globals_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("material"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: 8,
            ..Default::default()
        });
        let white_image = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 255, 255, 255]));
        let white = create_texture_group(&device, &queue, &texture_layout, &sampler, &white_image);
        let (instances, instance_group) = create_instances(&device, &instance_layout, 256);
        // Until the skybox has loaded, the sky is the studio backdrop colour.
        let backdrop = image::RgbaImage::from_pixel(1, 1, image::Rgba([52, 54, 60, 255]));
        let sky_group = create_cube_group(&device, &queue, &sky_layout, &sampler, &std::array::from_fn(|_| backdrop.clone()));

        let backdrop_mesh = crate::mesh::cyclorama();
        let backdrop = GpuMesh {
            vertices: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("backdrop"),
                contents: bytemuck::cast_slice(&backdrop_mesh.vertices),
                usage: wgpu::BufferUsages::VERTEX,
            }),
            indices: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("backdrop"),
                contents: bytemuck::cast_slice(&backdrop_mesh.indices),
                usage: wgpu::BufferUsages::INDEX,
            }),
            count: backdrop_mesh.indices.len() as u32,
            center: Vec3::ZERO,
        };

        Ok(Self {
            device,
            queue,
            opaque_pipeline,
            transparent_pipeline,
            shadow_pipeline,
            ssao_pipeline,
            blur_pipeline,
            composite_pipeline,
            globals,
            globals_group,
            shadow_globals_group,
            instance_layout,
            instances,
            instance_capacity: 256,
            instance_group,
            texture_layout,
            post_layout,
            sky_layout,
            sky_group,
            sky_loaded: false,
            sampler,
            white,
            shadow_view,
            backdrop,
            targets: None,
            generation: u64::MAX,
            meshes: Vec::new(),
            material_groups: Vec::new(),
            loader: TextureLoader { base_dir: world_dir, ..Default::default() },
        })
    }

    fn sync_scene(&mut self, scene: &Scene, generation: u64) {
        if generation != self.generation {
            self.generation = generation;
            self.meshes = scene
                .meshes
                .iter()
                .map(|m| {
                    let (lo, hi) = m.vertices.iter().fold((Vec3::MAX, Vec3::MIN), |(lo, hi), v| {
                        let p = Vec3::from(v.position);
                        (lo.min(p), hi.max(p))
                    });
                    GpuMesh {
                        vertices: self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: None,
                            contents: bytemuck::cast_slice(&m.vertices),
                            usage: wgpu::BufferUsages::VERTEX,
                        }),
                        indices: self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: None,
                            contents: bytemuck::cast_slice(&m.indices),
                            usage: wgpu::BufferUsages::INDEX,
                        }),
                        count: m.indices.len() as u32,
                        center: (lo + hi) * 0.5,
                    }
                })
                .collect();
            self.material_groups = vec![None; scene.materials.len()];
            for m in &scene.materials {
                if let Some(url) = &m.base_color_map {
                    self.loader.request(url);
                }
            }
            self.sky_loaded = false;
            for url in scene.sky.iter().flatten() {
                self.loader.request(url);
            }
        }
        if !self.sky_loaded {
            if let Some(faces) = &scene.sky {
                let loaded: Vec<_> = faces.iter().filter_map(|u| self.loader.ready(u)).collect();
                if loaded.len() == 6 {
                    let size = loaded.iter().map(|i| i.width().min(i.height())).min().unwrap_or(1).min(1024);
                    let faces: [image::RgbaImage; 6] = std::array::from_fn(|i| {
                        image::imageops::resize(&*loaded[i], size, size, image::imageops::FilterType::Triangle)
                    });
                    self.sky_group = create_cube_group(&self.device, &self.queue, &self.sky_layout, &self.sampler, &faces);
                    self.sky_loaded = true;
                }
            }
        }
        // Upload textures that finished downloading.
        for (i, m) in scene.materials.iter().enumerate() {
            if self.material_groups[i].is_none() {
                if let Some(img) = m.base_color_map.as_deref().and_then(|u| self.loader.ready(u)) {
                    self.material_groups[i] = Some(create_texture_group(
                        &self.device,
                        &self.queue,
                        &self.texture_layout,
                        &self.sampler,
                        &img,
                    ));
                }
            }
        }
    }

    fn ensure_targets(&mut self, width: u32, height: u32) {
        if self.targets.as_ref().is_some_and(|t| t.width == width && t.height == height) {
            return;
        }
        let size = wgpu::Extent3d { width, height, depth_or_array_layers: 1 };
        let view = |label, samples, format, usage: wgpu::TextureUsages| {
            self.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size,
                    mip_level_count: 1,
                    sample_count: samples,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        let attach = wgpu::TextureUsages::RENDER_ATTACHMENT;
        let readable = attach | wgpu::TextureUsages::TEXTURE_BINDING;
        let msaa = ["direct", "ambient", "normal"].map(|l| view(l, SAMPLES, HDR_FORMAT, attach));
        let resolved =
            ["direct resolved", "ambient resolved", "normal resolved"].map(|l| view(l, 1, HDR_FORMAT, readable));
        let depth = view("depth", SAMPLES, DEPTH_FORMAT, attach);
        let ao_raw = view("ao raw", 1, AO_FORMAT, readable);
        let ao = view("ao", 1, AO_FORMAT, readable);
        let out = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("out"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OUT_FORMAT,
            usage: attach | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let out_view = out.create_view(&Default::default());
        let post_group = |label, a: &wgpu::TextureView, b: &wgpu::TextureView, c: &wgpu::TextureView, d: &wgpu::TextureView| {
            self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some(label),
                layout: &self.post_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.globals.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(a) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(b) },
                    wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(c) },
                    wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(d) },
                ],
            })
        };
        let ssao_group = post_group("ssao", &resolved[2], &resolved[2], &resolved[2], &resolved[2]);
        let blur_group = post_group("blur", &ao_raw, &resolved[2], &resolved[2], &resolved[2]);
        let composite_group = post_group("composite", &resolved[0], &resolved[1], &ao, &resolved[2]);
        let padded_row = (width * 4).div_ceil(256) * 256;
        let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: padded_row as u64 * height as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        self.targets = Some(Targets {
            width,
            height,
            msaa,
            depth,
            resolved,
            ao_raw,
            ao,
            out,
            out_view,
            ssao_group,
            blur_group,
            composite_group,
            readback,
            padded_row,
        });
    }

    /// Renders the scene and returns tightly packed BGRA pixels.
    /// `robot` is the scene node of the robot; the studio view shows only that subtree.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        scene: &Scene,
        generation: u64,
        camera: &Camera,
        width: u32,
        height: u32,
        view: View,
        robot: Option<usize>,
    ) -> Vec<u8> {
        let (width, height) = (width.max(16), height.max(16));
        self.sync_scene(scene, generation);
        self.ensure_targets(width, height);

        let eye = camera.eye();
        let robot_world = robot.and_then(|r| scene.world_matrix_of(r));
        let studio = match (view, robot_world) {
            (View::Studio { dark }, Some(m)) => Some((dark, m)),
            _ => None,
        };
        let mut draws = match (studio, robot) {
            (Some(_), Some(r)) => scene.draws_under(r),
            _ => scene.draws(),
        };
        // Dollhouse cutaway (world view): hide walls whose outside faces the camera.
        let wall_centers: Vec<Vec2> = draws
            .iter()
            .filter(|d| d.cutaway)
            .map(|d| d.world.transform_point3(self.meshes[d.mesh].center).truncate())
            .collect();
        if studio.is_none() && !wall_centers.is_empty() {
            let room = wall_centers.iter().sum::<Vec2>() / wall_centers.len() as f32;
            // Like The Sims: the roof disappears once you zoom in on the house.
            let near_house = (eye - room.extend(1.4)).length() < 30.0;
            let meshes = &self.meshes;
            draws.retain(|d| {
                if d.roof {
                    return !near_house;
                }
                if !d.cutaway {
                    return true;
                }
                let c = d.world.transform_point3(meshes[d.mesh].center).truncate();
                (eye.truncate() - c).dot(c - room) <= 0.0
            });
        }

        // One extra instance slot for the studio backdrop.
        if draws.len() as u64 + 1 > self.instance_capacity {
            let capacity = (draws.len() as u64 + 1).next_power_of_two();
            (self.instances, self.instance_group) = create_instances(&self.device, &self.instance_layout, capacity);
            self.instance_capacity = capacity;
        }
        let mut instance_bytes = vec![0u8; (draws.len() + 1) * INSTANCE_STRIDE as usize];
        for (i, d) in draws.iter().enumerate() {
            let m: &Material = &scene.materials[d.material];
            let textured = m.base_color_map.is_some() && self.material_groups[d.material].is_some();
            let inst = Instance {
                model: d.world.to_cols_array_2d(),
                normal: d.world.inverse().transpose().to_cols_array_2d(),
                base_color: Vec4::from((m.base_color, 1.0 - m.transparency)).to_array(),
                params: [m.roughness, m.metalness, textured as u32 as f32, 0.0],
                emissive: Vec4::from((m.emissive, 0.0)).to_array(),
                uv_scale: [m.texture_scale.x, m.texture_scale.y, 0.0, 0.0],
            };
            let start = i * INSTANCE_STRIDE as usize;
            instance_bytes[start..start + std::mem::size_of::<Instance>()]
                .copy_from_slice(bytemuck::bytes_of(&inst));
        }
        // The backdrop stands where the robot is, turned with it, and stays level.
        let backdrop_slot = draws.len();
        if let Some((dark, robot)) = studio {
            let facing = robot.x_axis.truncate();
            let yaw = facing.y.atan2(facing.x);
            let p = robot.w_axis.truncate();
            let model = Mat4::from_translation(Vec3::new(p.x, p.y, 0.0)) * Mat4::from_rotation_z(yaw);
            let (color, roughness) = if dark { (Vec3::splat(0.014), 0.6) } else { (Vec3::new(0.86, 0.85, 0.83), 0.7) };
            let inst = Instance {
                model: model.to_cols_array_2d(),
                normal: model.inverse().transpose().to_cols_array_2d(),
                base_color: Vec4::from((color, 1.0)).to_array(),
                params: [roughness, 0.0, 0.0, 0.0],
                emissive: [0.0; 4],
                uv_scale: [1.0, 1.0, 0.0, 0.0],
            };
            let start = backdrop_slot * INSTANCE_STRIDE as usize;
            instance_bytes[start..start + std::mem::size_of::<Instance>()]
                .copy_from_slice(bytemuck::bytes_of(&inst));
        }
        self.queue.write_buffer(&self.instances, 0, &instance_bytes);

        // World: warm afternoon sun over the window wall, shadow box following the view.
        // Studio: a key light from the camera's upper left, shadow box tight around the robot.
        let (sun_dir, sun_intensity, sun_color, focus, extent) = match studio {
            Some((dark, robot)) => {
                let center = robot.w_axis.truncate() * Vec3::new(1.0, 1.0, 0.0) + Vec3::Z * 0.3;
                let forward = (eye - center).normalize();
                let left = Vec3::Z.cross(forward).normalize();
                let key = center + forward * 1.5 + left * 1.1 + Vec3::Z * 1.7;
                let intensity = if dark { 3.1 } else { 2.4 };
                ((center - key).normalize(), intensity, [1.0, 0.97, 0.93, 0.0], center, 2.5)
            }
            None => (
                Vec3::new(-0.3, -0.55, -0.78).normalize(),
                2.4,
                [1.0, 0.9, 0.78, 0.0],
                camera.target,
                (camera.distance * 0.9).clamp(5.0, 45.0),
            ),
        };
        let light_view = glam::camera::rh::view::look_at_mat4(focus - sun_dir * 60.0, focus, Vec3::Z);
        let light_proj = glam::camera::rh::proj::directx::orthographic(-extent, extent, -extent, extent, 1.0, 140.0);
        let mut light_pos = [[0.0; 4]; MAX_LIGHTS];
        let mut light_color = [[0.0; 4]; MAX_LIGHTS];
        let mut light_count = 0;
        if let Some((dark, robot)) = studio {
            // Two rim lights behind the robot draw a bright edge around its silhouette.
            let center = robot.w_axis.truncate() * Vec3::new(1.0, 1.0, 0.0);
            let forward = (eye - center).with_z(0.0).normalize_or_zero();
            let left = Vec3::Z.cross(forward).normalize_or_zero();
            let (color, strength) = if dark { ([0.8, 0.88, 1.0], 3.2) } else { ([1.0, 1.0, 1.0], 1.2) };
            for side in [-1.0, 1.0] {
                let p = center - forward * 0.75 + left * side * 0.65 + Vec3::Z * 0.95;
                light_pos[light_count] = [p.x, p.y, p.z, 4.0];
                light_color[light_count] = [color[0] * strength, color[1] * strength, color[2] * strength, 0.0];
                light_count += 1;
            }
            // A soft pool of light on the wall behind the robot (backdrop frame: wall at x = -3.6).
            let facing = robot.x_axis.truncate().with_z(0.0).normalize_or_zero();
            let wash = center - facing * 2.9 + Vec3::Z * 1.25;
            let wash_strength = if dark { 3.4 } else { 0.8 };
            light_pos[light_count] = [wash.x, wash.y, wash.z, 2.4];
            light_color[light_count] = [wash_strength, wash_strength * 0.97, wash_strength * 0.93, 0.0];
            light_count += 1;
            // Soft fill from the camera side, slightly low, so the face isn't lost under the visor.
            let fill = center + forward * 1.4 + Vec3::Z * 0.35;
            let fill_strength = if dark { 1.1 } else { 0.0 };
            light_pos[light_count] = [fill.x, fill.y, fill.z, 4.0];
            light_color[light_count] = [fill_strength, fill_strength, fill_strength * 1.02, 0.0];
            light_count += 1;
        }
        for light in scene.point_lights.iter().take(if studio.is_some() { 0 } else { MAX_LIGHTS }) {
            let Some(world) = scene.world_matrix_of(light.node) else { continue };
            let p = world.transform_point3(light.location);
            light_pos[light_count] = [p.x, p.y, p.z, 7.0];
            let c = light.color * light.intensity * 0.9;
            light_color[light_count] = [c.x, c.y, c.z, 0.0];
            light_count += 1;
        }
        let view = camera.view();
        let view_proj = Camera::proj(width as f32 / height as f32) * view;
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            view: view.to_cols_array_2d(),
            light_view_proj: (light_proj * light_view).to_cols_array_2d(),
            camera_pos: Vec4::from((eye, 1.0)).to_array(),
            sun_dir: Vec4::from((sun_dir, sun_intensity)).to_array(),
            sun_color,
            // In the studio, "sky" is a big overhead softbox: it is what the glossy shell reflects.
            sky: match studio {
                Some((true, _)) => [0.2, 0.21, 0.23, 0.2],
                Some((false, _)) => [0.96, 0.96, 0.98, 0.4],
                None => [0.58, 0.66, 0.8, 0.42],
            },
            ground: match studio {
                Some((true, _)) => [0.012, 0.013, 0.016, 0.0],
                Some((false, _)) => [0.78, 0.78, 0.8, 0.0],
                None => [0.46, 0.36, 0.27, 0.0],
            },
            light_pos,
            light_color,
            params: [light_count as f32, width as f32, height as f32, (FOV_Y.to_radians() * 0.5).tan()],
            fog: match studio {
                Some((true, _)) => [0.012, 0.013, 0.015, 4.5],
                Some((false, _)) => [0.93, 0.93, 0.95, 3.0],
                None => [0.0, 0.0, 0.0, 16.0],
            },
            fog_params: match studio {
                Some((true, _)) => [8.0, 1.0, 0.0, 0.0],
                Some((false, _)) => [9.0, 1.0, 0.0, 0.0],
                None => [26.0, 0.0, 0.0, 0.0],
            },
        };
        self.queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        let targets = self.targets.as_ref().unwrap();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("shadow"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.shadow_view,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.shadow_pipeline);
            pass.set_bind_group(0, &self.shadow_globals_group, &[]);
            for (i, d) in draws.iter().enumerate() {
                if scene.materials[d.material].transparency > 0.5 {
                    continue;
                }
                let mesh = &self.meshes[d.mesh];
                pass.set_bind_group(1, &self.instance_group, &[(i as u64 * INSTANCE_STRIDE) as u32]);
                pass.set_bind_group(2, self.material_groups[d.material].as_ref().unwrap_or(&self.white), &[]);
                pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.count, 0, 0..1);
            }
            if studio.is_some() {
                pass.set_bind_group(1, &self.instance_group, &[(backdrop_slot as u64 * INSTANCE_STRIDE) as u32]);
                pass.set_bind_group(2, &self.white, &[]);
                pass.set_vertex_buffer(0, self.backdrop.vertices.slice(..));
                pass.set_index_buffer(self.backdrop.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..self.backdrop.count, 0, 0..1);
            }
        }
        {
            // Backdrop outside the room: a dim neutral studio grey. Normal target: far depth.
            let clears = [
                wgpu::Color { r: 0.035, g: 0.038, b: 0.045, a: 1.0 },
                wgpu::Color::TRANSPARENT,
                wgpu::Color { r: 0.0, g: 0.0, b: 1.0, a: 1000.0 },
            ];
            let color_attachments: Vec<Option<wgpu::RenderPassColorAttachment>> = (0..3)
                .map(|i| {
                    Some(wgpu::RenderPassColorAttachment {
                        view: &targets.msaa[i],
                        depth_slice: None,
                        resolve_target: Some(&targets.resolved[i]),
                        ops: wgpu::Operations { load: wgpu::LoadOp::Clear(clears[i]), store: wgpu::StoreOp::Discard },
                    })
                })
                .collect();
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene"),
                color_attachments: &color_attachments,
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &targets.depth,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_bind_group(0, &self.globals_group, &[]);
            // Opaque first, then transparent so blending sees what is behind it.
            for transparent in [false, true] {
                pass.set_pipeline(if transparent { &self.transparent_pipeline } else { &self.opaque_pipeline });
                for (i, d) in draws.iter().enumerate() {
                    if (scene.materials[d.material].transparency > 0.0) != transparent {
                        continue;
                    }
                    let mesh = &self.meshes[d.mesh];
                    pass.set_bind_group(1, &self.instance_group, &[(i as u64 * INSTANCE_STRIDE) as u32]);
                    pass.set_bind_group(2, self.material_groups[d.material].as_ref().unwrap_or(&self.white), &[]);
                    pass.set_vertex_buffer(0, mesh.vertices.slice(..));
                    pass.set_index_buffer(mesh.indices.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..mesh.count, 0, 0..1);
                }
                if !transparent && studio.is_some() {
                    pass.set_bind_group(1, &self.instance_group, &[(backdrop_slot as u64 * INSTANCE_STRIDE) as u32]);
                    pass.set_bind_group(2, &self.white, &[]);
                    pass.set_vertex_buffer(0, self.backdrop.vertices.slice(..));
                    pass.set_index_buffer(self.backdrop.indices.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..self.backdrop.count, 0, 0..1);
                }
            }
        }
        for (label, pipeline, group, target) in [
            ("ssao", &self.ssao_pipeline, &targets.ssao_group, &targets.ao_raw),
            ("blur", &self.blur_pipeline, &targets.blur_group, &targets.ao),
            ("composite", &self.composite_pipeline, &targets.composite_group, &targets.out_view),
        ] {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(label),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, group, &[]);
            pass.set_bind_group(1, &self.sky_group, &[]);
            pass.draw(0..3, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &targets.out,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &targets.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(targets.padded_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit([encoder.finish()]);

        let slice = targets.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        {
            let data = slice.get_mapped_range().expect("readback buffer is mapped");
            for row in data.chunks(targets.padded_row as usize).take(height as usize) {
                pixels.extend_from_slice(&row[..(width * 4) as usize]);
            }
        }
        targets.readback.unmap();
        pixels
    }
}

fn create_instances(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    capacity: u64,
) -> (wgpu::Buffer, wgpu::BindGroup) {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("instances"),
        size: capacity * INSTANCE_STRIDE,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("instances"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                buffer: &buffer,
                offset: 0,
                size: wgpu::BufferSize::new(std::mem::size_of::<Instance>() as u64),
            }),
        }],
    });
    (buffer, group)
}

fn create_texture_group(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    img: &image::RgbaImage,
) -> wgpu::BindGroup {
    let size = wgpu::Extent3d { width: img.width(), height: img.height(), depth_or_array_layers: 1 };
    let mip_level_count = size.max_mips(wgpu::TextureDimension::D2);
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("material"),
        size,
        mip_level_count,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let mut level = img.clone();
    for mip in 0..mip_level_count {
        if mip > 0 {
            let (w, h) = ((level.width() / 2).max(1), (level.height() / 2).max(1));
            level = image::imageops::resize(&level, w, h, image::imageops::FilterType::Triangle);
        }
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: mip,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            level.as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * level.width()),
                rows_per_image: Some(level.height()),
            },
            wgpu::Extent3d { width: level.width(), height: level.height(), depth_or_array_layers: 1 },
        );
    }
    let view = texture.create_view(&Default::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("material"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    })
}

/// A cube texture from six faces in Webots' order: right, left, top, bottom, front, back.
fn create_cube_group(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    faces: &[image::RgbaImage; 6],
) -> wgpu::BindGroup {
    let size = faces[0].width();
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sky"),
        size: wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 6 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    // wgpu cube layers are +X, -X, +Y, -Y, +Z, -Z. The shader looks up (x, z, -y) of the Z-up world
    // direction, so +Y is the sky's top and -Z its front.
    for (layer, face) in [0usize, 1, 2, 3, 5, 4].into_iter().enumerate() {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y: 0, z: layer as u32 },
                aspect: wgpu::TextureAspect::All,
            },
            faces[face].as_raw(),
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * size),
                rows_per_image: Some(size),
            },
            wgpu::Extent3d { width: size, height: size, depth_or_array_layers: 1 },
        );
    }
    let view = texture.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(wgpu::TextureViewDimension::Cube),
        ..Default::default()
    });
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("sky"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(sampler) },
        ],
    })
}
