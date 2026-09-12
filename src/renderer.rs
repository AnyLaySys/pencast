use std::borrow::Cow;
use std::num::NonZeroIsize;

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, Win32WindowHandle, WindowsDisplayHandle,
};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

use crate::protocol::Frames;

const SHADER: &str = r#"
struct Output {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@group(0) @binding(0) var image: texture_2d<f32>;
@group(0) @binding(1) var image_sampler: sampler;

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> Output {
    var positions = array<vec2<f32>, 3>(
        vec2(-1.0, -1.0),
        vec2(3.0, -1.0),
        vec2(-1.0, 3.0),
    );
    var uvs = array<vec2<f32>, 3>(
        vec2(0.0, 0.0),
        vec2(2.0, 0.0),
        vec2(0.0, 2.0),
    );
    return Output(vec4(positions[index], 0.0, 1.0), uvs[index]);
}

@fragment
fn fragment(output: Output) -> @location(0) vec4<f32> {
    return textureSample(image, image_sampler, vec2(1.0 - output.uv.y, 1.0 - output.uv.x));
}
"#;

pub(crate) struct Renderer {
    _instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    texture: Option<wgpu::Texture>,
    bind: Option<wgpu::BindGroup>,
    frame_width: u32,
    frame_height: u32,
    uploaded: Option<u64>,
}

impl Renderer {
    pub(crate) fn new(hwnd: HWND) -> Result<Self, String> {
        let (width, height) = client_size(hwnd)?;
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(descriptor);
        let window = Win32WindowHandle::new(
            NonZeroIsize::new(hwnd.0 as isize)
                .ok_or_else(|| String::from("Invalid window handle"))?,
        );
        let surface: wgpu::Surface<'static> = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: Some(RawDisplayHandle::Windows(WindowsDisplayHandle::new())),
                raw_window_handle: RawWindowHandle::Win32(window),
            })
        }
        .map_err(|error| format!("Creating D3D12 surface failed: {error}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .map_err(|error| format!("Selecting D3D12 adapter failed: {error}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&Default::default()))
            .map_err(|error| format!("Creating D3D12 device failed: {error}"))?;
        let mut config = surface
            .get_default_config(&adapter, width, height)
            .ok_or_else(|| String::from("D3D12 surface is unsupported"))?;
        config.present_mode = wgpu::PresentMode::Fifo;
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
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
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: None,
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(SHADER)),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: None,
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        Ok(Self {
            _instance: instance,
            surface,
            device,
            queue,
            config,
            pipeline,
            layout,
            sampler,
            texture: None,
            bind: None,
            frame_width: 0,
            frame_height: 0,
            uploaded: None,
        })
    }

    fn resize(&mut self, hwnd: HWND) -> Result<bool, String> {
        let (width, height) = client_size(hwnd)?;
        if width == 0 || height == 0 {
            return Ok(false);
        }
        if self.config.width != width || self.config.height != height {
            self.config.width = width;
            self.config.height = height;
            self.surface.configure(&self.device, &self.config);
        }
        Ok(true)
    }

    fn upload(&mut self, frames: &Frames) -> Result<(), String> {
        let width = u32::try_from(frames.width).map_err(|_| String::from("Invalid frame width"))?;
        let height =
            u32::try_from(frames.height).map_err(|_| String::from("Invalid frame height"))?;
        if self.frame_width != width || self.frame_height != height {
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            self.bind = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            }));
            self.texture = Some(texture);
            self.frame_width = width;
            self.frame_height = height;
            self.uploaded = None;
        }
        if self.uploaded != Some(frames.sequence) {
            let bytes = frames
                .width
                .checked_mul(frames.height)
                .and_then(|value| value.checked_mul(4))
                .ok_or_else(|| String::from("Invalid frame size"))?;
            let data = &frames.buffers[frames.front];
            if data.len() != bytes {
                return Err(String::from("Invalid frame buffer"));
            }
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: self
                        .texture
                        .as_ref()
                        .ok_or_else(|| String::from("Missing frame texture"))?,
                    mip_level: 0,
                    origin: Default::default(),
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            self.uploaded = Some(frames.sequence);
        }
        Ok(())
    }

    pub(crate) fn render(&mut self, hwnd: HWND, frames: &Frames) -> Result<(), String> {
        if !self.resize(hwnd)? {
            return Ok(());
        }
        if frames.width != 0 && frames.height != 0 {
            self.upload(frames)?;
        }
        let (output, suboptimal) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(output) => (output, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(output) => (output, true),
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                return Err(String::from("D3D12 surface was lost"));
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(String::from("D3D12 surface validation failed"));
            }
        };
        let view = output.texture.create_view(&Default::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(bind) = self.bind.as_ref() {
                let scale = (self.config.width as f64 / self.frame_height as f64)
                    .min(self.config.height as f64 / self.frame_width as f64);
                let width = self.frame_height as f64 * scale;
                let height = self.frame_width as f64 * scale;
                pass.set_viewport(
                    (self.config.width as f64 - width) as f32 / 2.0,
                    (self.config.height as f64 - height) as f32 / 2.0,
                    width as f32,
                    height as f32,
                    0.0,
                    1.0,
                );
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, bind, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(output);
        if suboptimal {
            self.surface.configure(&self.device, &self.config);
        }
        Ok(())
    }
}

fn client_size(hwnd: HWND) -> Result<(u32, u32), String> {
    let mut rect = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut rect).map_err(|error| error.to_string())?;
    }
    Ok((
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    ))
}
