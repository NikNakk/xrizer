//! Draws a SteamVR stage override (OBJ mesh + texture) into the compositor's
//! stereo swapchain with Vulkan, matching the D3D11 stage renderer's look.

use crate::stage::{StageAsset, StageVertex, projection_matrix, view_matrix};
use ash::vk;
use openxr as xr;

const NEAR: f32 = 0.05;
const FAR: f32 = 1000.0;

/// Device handles the renderer needs. The device and queue belong to the
/// application; the renderer only creates and destroys its own objects.
pub(super) struct StageDevice<'a> {
    pub instance: &'a ash::Instance,
    pub physical_device: vk::PhysicalDevice,
    pub device: &'a ash::Device,
    pub queue: vk::Queue,
    pub queue_family_index: u32,
}

struct Buffer {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
}

struct Image {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
}

struct EyeTarget {
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SettingsBlock {
    primary_color: [f32; 4],
    secondary_color: [f32; 4],
    effects: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PushBlock {
    model: [f32; 16],
    view_projection: [f32; 16],
}

pub(super) struct StageRenderer {
    device: ash::Device,
    queue: vk::Queue,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    pub stage_id: u64,
    pub target_format: vk::Format,
    clear_color: [f32; 4],
    depth_format: vk::Format,
    render_pass: vk::RenderPass,
    set_layout: vk::DescriptorSetLayout,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    descriptor_pool: vk::DescriptorPool,
    descriptor_set: vk::DescriptorSet,
    sampler: vk::Sampler,
    vertices: Buffer,
    indices: Buffer,
    index_count: u32,
    settings: Buffer,
    texture: Image,
    depth: Option<(Image, vk::Extent2D)>,
    target_images: Vec<vk::Image>,
    targets: Vec<Option<[EyeTarget; 2]>>,
    command_pool: vk::CommandPool,
    commands: Vec<(vk::CommandBuffer, vk::Fence)>,
}

impl StageRenderer {
    pub fn new(
        ctx: &StageDevice,
        stage: &StageAsset,
        target_format: vk::Format,
    ) -> Result<Self, String> {
        let device = ctx.device.clone();
        let memory_properties = unsafe {
            ctx.instance
                .get_physical_device_memory_properties(ctx.physical_device)
        };
        let depth_format = [vk::Format::D32_SFLOAT, vk::Format::D16_UNORM]
            .into_iter()
            .find(|format| {
                let props = unsafe {
                    ctx.instance
                        .get_physical_device_format_properties(ctx.physical_device, *format)
                };
                props
                    .optimal_tiling_features
                    .contains(vk::FormatFeatureFlags::DEPTH_STENCIL_ATTACHMENT)
            })
            .ok_or("no supported depth format for the stage")?;

        let command_pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
                    .queue_family_index(ctx.queue_family_index),
                None,
            )
        }
        .map_err(|e| format!("stage command pool: {e}"))?;

        // From here on, a partly built renderer cleans itself up through Drop.
        let mut renderer = Self {
            device,
            queue: ctx.queue,
            memory_properties,
            stage_id: stage.id,
            target_format,
            clear_color: stage.settings.secondary_color,
            depth_format,
            render_pass: vk::RenderPass::null(),
            set_layout: vk::DescriptorSetLayout::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            pipeline: vk::Pipeline::null(),
            descriptor_pool: vk::DescriptorPool::null(),
            descriptor_set: vk::DescriptorSet::null(),
            sampler: vk::Sampler::null(),
            vertices: Buffer::null(),
            indices: Buffer::null(),
            index_count: stage.indices.len() as u32,
            settings: Buffer::null(),
            texture: Image::null(),
            depth: None,
            target_images: Vec::new(),
            targets: Vec::new(),
            command_pool,
            commands: Vec::new(),
        };

        if stage.settings.wireframe {
            log::warn!("stage override requested wireframe; drawing it solid");
        }
        if stage.settings.fresnel_strength != 0.0 {
            log::warn!("stage override requested a fresnel effect; it is not drawn");
        }

        renderer.vertices = renderer.host_buffer(
            vk::BufferUsageFlags::VERTEX_BUFFER,
            as_bytes(&stage.vertices),
        )?;
        renderer.indices =
            renderer.host_buffer(vk::BufferUsageFlags::INDEX_BUFFER, as_bytes(&stage.indices))?;
        let settings = SettingsBlock {
            primary_color: stage.settings.primary_color,
            secondary_color: stage.settings.secondary_color,
            effects: [
                u32::from(stage.settings.greyscale) as f32,
                stage.settings.vignette_inner_radius,
                stage.settings.vignette_outer_radius,
                // D3D11 samples the texture as sRGB; a single-channel upload
                // decodes it in the shader instead.
                u32::from(stage.texture_luminance) as f32,
            ],
        };
        renderer.settings = renderer.host_buffer(
            vk::BufferUsageFlags::UNIFORM_BUFFER,
            as_bytes(std::slice::from_ref(&settings)),
        )?;
        let mip_levels = renderer.upload_texture(ctx, stage)?;
        renderer.create_pipeline(stage, mip_levels)?;
        Ok(renderer)
    }

    fn find_memory(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> Result<u32, String> {
        (0..self.memory_properties.memory_type_count)
            .find(|&i| {
                bits & (1 << i) != 0
                    && self.memory_properties.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
            .ok_or_else(|| format!("no memory type with {flags:?}"))
    }

    fn host_buffer(&self, usage: vk::BufferUsageFlags, data: &[u8]) -> Result<Buffer, String> {
        let d = &self.device;
        unsafe {
            let buffer = d
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(data.len().max(4) as u64)
                        .usage(usage)
                        .sharing_mode(vk::SharingMode::EXCLUSIVE),
                    None,
                )
                .map_err(|e| format!("stage buffer: {e}"))?;
            let req = d.get_buffer_memory_requirements(buffer);
            let memory = match self
                .find_memory(
                    req.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
                )
                .and_then(|index| {
                    d.allocate_memory(
                        &vk::MemoryAllocateInfo::default()
                            .allocation_size(req.size)
                            .memory_type_index(index),
                        None,
                    )
                    .map_err(|e| format!("stage buffer memory: {e}"))
                }) {
                Ok(memory) => memory,
                Err(e) => {
                    d.destroy_buffer(buffer, None);
                    return Err(e);
                }
            };
            let result = (|| {
                d.bind_buffer_memory(buffer, memory, 0)
                    .map_err(|e| format!("bind stage buffer: {e}"))?;
                let ptr = d
                    .map_memory(memory, 0, req.size, vk::MemoryMapFlags::empty())
                    .map_err(|e| format!("map stage buffer: {e}"))?;
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.cast::<u8>(), data.len());
                d.unmap_memory(memory);
                Ok(())
            })();
            let buffer = Buffer { buffer, memory };
            if let Err(e) = result {
                buffer.destroy(d);
                return Err(e);
            }
            Ok(buffer)
        }
    }

    /// Uploads the texture with a full mip chain where the format allows it,
    /// so the large diorama textures do not shimmer. Returns the mip count.
    fn upload_texture(&mut self, ctx: &StageDevice, stage: &StageAsset) -> Result<u32, String> {
        let (width, height) = (stage.texture_width, stage.texture_height);
        let pixels = width as usize * height as usize;
        if stage.texture_rgba.len() < pixels * 4 {
            return Err("stage texture data is shorter than its size".into());
        }
        let (format, swizzle, data) = if stage.texture_luminance {
            let red: Vec<u8> = stage
                .texture_rgba
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| p[0])
                .collect();
            let r = vk::ComponentSwizzle::R;
            (
                vk::Format::R8_UNORM,
                vk::ComponentMapping {
                    r,
                    g: r,
                    b: r,
                    a: vk::ComponentSwizzle::ONE,
                },
                red,
            )
        } else {
            (
                vk::Format::R8G8B8A8_SRGB,
                vk::ComponentMapping::default(),
                stage.texture_rgba[..pixels * 4].to_vec(),
            )
        };

        let features = unsafe {
            ctx.instance
                .get_physical_device_format_properties(ctx.physical_device, format)
        }
        .optimal_tiling_features;
        let can_mip = features.contains(
            vk::FormatFeatureFlags::BLIT_SRC
                | vk::FormatFeatureFlags::BLIT_DST
                | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
        );
        let mip_levels = if can_mip {
            32 - width.max(height).leading_zeros()
        } else {
            1
        };

        let d = self.device.clone();
        let staging = self.host_buffer(vk::BufferUsageFlags::TRANSFER_SRC, &data)?;
        drop(data);
        let result = (|| unsafe {
            let image = d
                .create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(format)
                        .extent(vk::Extent3D {
                            width,
                            height,
                            depth: 1,
                        })
                        .mip_levels(mip_levels)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(
                            vk::ImageUsageFlags::SAMPLED
                                | vk::ImageUsageFlags::TRANSFER_DST
                                | vk::ImageUsageFlags::TRANSFER_SRC,
                        )
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    None,
                )
                .map_err(|e| format!("stage texture: {e}"))?;
            self.texture.image = image;
            let req = d.get_image_memory_requirements(image);
            let memory_type =
                self.find_memory(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
            self.texture.memory = d
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(memory_type),
                    None,
                )
                .map_err(|e| format!("stage texture memory: {e}"))?;
            d.bind_image_memory(image, self.texture.memory, 0)
                .map_err(|e| format!("bind stage texture: {e}"))?;

            self.one_time_commands(|cmd| {
                let range = |base_mip_level, level_count| vk::ImageSubresourceRange {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    base_mip_level,
                    level_count,
                    base_array_layer: 0,
                    layer_count: 1,
                };
                let barrier = |level, levels, old, new, src, dst| vk::ImageMemoryBarrier {
                    src_access_mask: src,
                    dst_access_mask: dst,
                    old_layout: old,
                    new_layout: new,
                    src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                    dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                    image,
                    subresource_range: range(level, levels),
                    ..Default::default()
                };
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier(
                        0,
                        mip_levels,
                        vk::ImageLayout::UNDEFINED,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        vk::AccessFlags::empty(),
                        vk::AccessFlags::TRANSFER_WRITE,
                    )],
                );
                d.cmd_copy_buffer_to_image(
                    cmd,
                    staging.buffer,
                    image,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[vk::BufferImageCopy {
                        image_subresource: vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        },
                        image_extent: vk::Extent3D {
                            width,
                            height,
                            depth: 1,
                        },
                        ..Default::default()
                    }],
                );
                let mip_size = |level: u32| ((width >> level).max(1), (height >> level).max(1));
                for level in 1..mip_levels {
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[barrier(
                            level - 1,
                            1,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            vk::AccessFlags::TRANSFER_WRITE,
                            vk::AccessFlags::TRANSFER_READ,
                        )],
                    );
                    let (sw, sh) = mip_size(level - 1);
                    let (dw, dh) = mip_size(level);
                    let layers = |mip_level| vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level,
                        base_array_layer: 0,
                        layer_count: 1,
                    };
                    d.cmd_blit_image(
                        cmd,
                        image,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        image,
                        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                        &[vk::ImageBlit {
                            src_subresource: layers(level - 1),
                            src_offsets: [
                                vk::Offset3D::default(),
                                vk::Offset3D {
                                    x: sw as i32,
                                    y: sh as i32,
                                    z: 1,
                                },
                            ],
                            dst_subresource: layers(level),
                            dst_offsets: [
                                vk::Offset3D::default(),
                                vk::Offset3D {
                                    x: dw as i32,
                                    y: dh as i32,
                                    z: 1,
                                },
                            ],
                        }],
                        vk::Filter::LINEAR,
                    );
                }
                let mut finals = Vec::new();
                if mip_levels > 1 {
                    finals.push(barrier(
                        0,
                        mip_levels - 1,
                        vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        vk::AccessFlags::TRANSFER_READ,
                        vk::AccessFlags::SHADER_READ,
                    ));
                }
                finals.push(barrier(
                    mip_levels - 1,
                    1,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::SHADER_READ,
                ));
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &finals,
                );
            })?;

            self.texture.view = d
                .create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format)
                        .components(swizzle)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: 0,
                            level_count: mip_levels,
                            base_array_layer: 0,
                            layer_count: 1,
                        }),
                    None,
                )
                .map_err(|e| format!("stage texture view: {e}"))?;
            Ok(())
        })();
        staging.destroy(&d);
        result.map(|()| mip_levels)
    }

    /// Records, submits and waits for a one-off command buffer.
    fn one_time_commands(&self, record: impl FnOnce(vk::CommandBuffer)) -> Result<(), String> {
        let d = &self.device;
        unsafe {
            let cmd = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(self.command_pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .map_err(|e| format!("stage upload commands: {e}"))?[0];
            let fence = d
                .create_fence(&vk::FenceCreateInfo::default(), None)
                .map_err(|e| format!("stage upload fence: {e}"))?;
            let result = (|| {
                d.begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .map_err(|e| format!("begin stage upload: {e}"))?;
                record(cmd);
                d.end_command_buffer(cmd)
                    .map_err(|e| format!("end stage upload: {e}"))?;
                d.queue_submit(
                    self.queue,
                    &[vk::SubmitInfo::default().command_buffers(&[cmd])],
                    fence,
                )
                .map_err(|e| format!("submit stage upload: {e}"))?;
                d.wait_for_fences(&[fence], true, u64::MAX)
                    .map_err(|e| format!("wait for stage upload: {e}"))
            })();
            d.destroy_fence(fence, None);
            d.free_command_buffers(self.command_pool, &[cmd]);
            result
        }
    }

    fn create_pipeline(&mut self, stage: &StageAsset, mip_levels: u32) -> Result<(), String> {
        let d = self.device.clone();
        unsafe {
            let attachments = [
                vk::AttachmentDescription {
                    format: self.target_format,
                    samples: vk::SampleCountFlags::TYPE_1,
                    load_op: vk::AttachmentLoadOp::CLEAR,
                    store_op: vk::AttachmentStoreOp::STORE,
                    stencil_load_op: vk::AttachmentLoadOp::DONT_CARE,
                    stencil_store_op: vk::AttachmentStoreOp::DONT_CARE,
                    initial_layout: vk::ImageLayout::UNDEFINED,
                    // OpenXR expects released Vulkan images in this layout.
                    final_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                    ..Default::default()
                },
                vk::AttachmentDescription {
                    format: self.depth_format,
                    samples: vk::SampleCountFlags::TYPE_1,
                    load_op: vk::AttachmentLoadOp::CLEAR,
                    store_op: vk::AttachmentStoreOp::DONT_CARE,
                    stencil_load_op: vk::AttachmentLoadOp::DONT_CARE,
                    stencil_store_op: vk::AttachmentStoreOp::DONT_CARE,
                    initial_layout: vk::ImageLayout::UNDEFINED,
                    final_layout: vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
                    ..Default::default()
                },
            ];
            let color_ref = [vk::AttachmentReference {
                attachment: 0,
                layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            }];
            let depth_ref = vk::AttachmentReference {
                attachment: 1,
                layout: vk::ImageLayout::DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
            };
            let subpass = vk::SubpassDescription::default()
                .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
                .color_attachments(&color_ref)
                .depth_stencil_attachment(&depth_ref);
            let stages = vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                | vk::PipelineStageFlags::EARLY_FRAGMENT_TESTS
                | vk::PipelineStageFlags::LATE_FRAGMENT_TESTS;
            let access = vk::AccessFlags::COLOR_ATTACHMENT_WRITE
                | vk::AccessFlags::DEPTH_STENCIL_ATTACHMENT_WRITE;
            let dependencies = [
                vk::SubpassDependency {
                    src_subpass: vk::SUBPASS_EXTERNAL,
                    dst_subpass: 0,
                    src_stage_mask: stages | vk::PipelineStageFlags::TRANSFER,
                    dst_stage_mask: stages,
                    src_access_mask: access | vk::AccessFlags::TRANSFER_WRITE,
                    dst_access_mask: access,
                    ..Default::default()
                },
                vk::SubpassDependency {
                    src_subpass: 0,
                    dst_subpass: vk::SUBPASS_EXTERNAL,
                    src_stage_mask: vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    dst_stage_mask: vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    src_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                    dst_access_mask: vk::AccessFlags::empty(),
                    ..Default::default()
                },
            ];
            self.render_pass = d
                .create_render_pass(
                    &vk::RenderPassCreateInfo::default()
                        .attachments(&attachments)
                        .subpasses(std::slice::from_ref(&subpass))
                        .dependencies(&dependencies),
                    None,
                )
                .map_err(|e| format!("stage render pass: {e}"))?;

            let bindings = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(1)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            ];
            self.set_layout = d
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .map_err(|e| format!("stage set layout: {e}"))?;
            self.descriptor_pool = d
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(1)
                        .pool_sizes(&[
                            vk::DescriptorPoolSize {
                                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                                descriptor_count: 1,
                            },
                            vk::DescriptorPoolSize {
                                ty: vk::DescriptorType::UNIFORM_BUFFER,
                                descriptor_count: 1,
                            },
                        ]),
                    None,
                )
                .map_err(|e| format!("stage descriptor pool: {e}"))?;
            let set = d
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(self.descriptor_pool)
                        .set_layouts(&[self.set_layout]),
                )
                .map_err(|e| format!("stage descriptor set: {e}"))?[0];
            self.sampler = d
                .create_sampler(
                    &vk::SamplerCreateInfo::default()
                        .mag_filter(vk::Filter::LINEAR)
                        .min_filter(vk::Filter::LINEAR)
                        .mipmap_mode(vk::SamplerMipmapMode::LINEAR)
                        .address_mode_u(vk::SamplerAddressMode::REPEAT)
                        .address_mode_v(vk::SamplerAddressMode::REPEAT)
                        .address_mode_w(vk::SamplerAddressMode::REPEAT)
                        .max_lod(mip_levels as f32),
                    None,
                )
                .map_err(|e| format!("stage sampler: {e}"))?;
            d.update_descriptor_sets(
                &[
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(0)
                        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                        .image_info(&[vk::DescriptorImageInfo {
                            sampler: self.sampler,
                            image_view: self.texture.view,
                            image_layout: vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                        }]),
                    vk::WriteDescriptorSet::default()
                        .dst_set(set)
                        .dst_binding(1)
                        .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                        .buffer_info(&[vk::DescriptorBufferInfo {
                            buffer: self.settings.buffer,
                            offset: 0,
                            range: size_of::<SettingsBlock>() as u64,
                        }]),
                ],
                &[],
            );
            self.descriptor_set = set;

            self.pipeline_layout = d
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&[self.set_layout])
                        .push_constant_ranges(&[vk::PushConstantRange {
                            stage_flags: vk::ShaderStageFlags::VERTEX,
                            offset: 0,
                            size: size_of::<PushBlock>() as u32,
                        }]),
                    None,
                )
                .map_err(|e| format!("stage pipeline layout: {e}"))?;

            let module = |bytes: &[u8]| {
                let code = ash::util::read_spv(&mut std::io::Cursor::new(bytes))
                    .map_err(|e| format!("stage shader: {e}"))?;
                d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
                    .map_err(|e| format!("stage shader module: {e}"))
            };
            let vert = module(include_bytes!(concat!(env!("OUT_DIR"), "/vert_stage.spv")))?;
            let frag = match module(include_bytes!(concat!(env!("OUT_DIR"), "/frag_stage.spv"))) {
                Ok(frag) => frag,
                Err(e) => {
                    d.destroy_shader_module(vert, None);
                    return Err(e);
                }
            };
            let shader_stages = [
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::VERTEX)
                    .module(vert)
                    .name(c"main"),
                vk::PipelineShaderStageCreateInfo::default()
                    .stage(vk::ShaderStageFlags::FRAGMENT)
                    .module(frag)
                    .name(c"main"),
            ];
            let vertex_bindings = [vk::VertexInputBindingDescription {
                binding: 0,
                stride: size_of::<StageVertex>() as u32,
                input_rate: vk::VertexInputRate::VERTEX,
            }];
            let vertex_attributes = [
                vk::VertexInputAttributeDescription {
                    location: 0,
                    binding: 0,
                    format: vk::Format::R32G32B32_SFLOAT,
                    offset: std::mem::offset_of!(StageVertex, position) as u32,
                },
                vk::VertexInputAttributeDescription {
                    location: 1,
                    binding: 0,
                    format: vk::Format::R32G32_SFLOAT,
                    offset: std::mem::offset_of!(StageVertex, tex_coord) as u32,
                },
            ];
            let vertex_input = vk::PipelineVertexInputStateCreateInfo::default()
                .vertex_binding_descriptions(&vertex_bindings)
                .vertex_attribute_descriptions(&vertex_attributes);
            let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
                .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
            let viewport = vk::PipelineViewportStateCreateInfo::default()
                .viewport_count(1)
                .scissor_count(1);
            // D3D11's default front face is clockwise on screen. With clip-space
            // Y flipped in the vertex shader, the framebuffer matches D3D's.
            let raster = vk::PipelineRasterizationStateCreateInfo::default()
                .polygon_mode(vk::PolygonMode::FILL)
                .cull_mode(if stage.settings.backface_culling {
                    vk::CullModeFlags::BACK
                } else {
                    vk::CullModeFlags::NONE
                })
                .front_face(vk::FrontFace::CLOCKWISE)
                .line_width(1.0);
            let multisample = vk::PipelineMultisampleStateCreateInfo::default()
                .rasterization_samples(vk::SampleCountFlags::TYPE_1);
            let depth = vk::PipelineDepthStencilStateCreateInfo::default()
                .depth_test_enable(true)
                .depth_write_enable(true)
                .depth_compare_op(vk::CompareOp::LESS);
            let blend_attachment = [vk::PipelineColorBlendAttachmentState {
                blend_enable: vk::FALSE,
                color_write_mask: vk::ColorComponentFlags::RGBA,
                ..Default::default()
            }];
            let blend =
                vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend_attachment);
            let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
            let dynamic =
                vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
            let info = vk::GraphicsPipelineCreateInfo::default()
                .stages(&shader_stages)
                .vertex_input_state(&vertex_input)
                .input_assembly_state(&assembly)
                .viewport_state(&viewport)
                .rasterization_state(&raster)
                .multisample_state(&multisample)
                .depth_stencil_state(&depth)
                .color_blend_state(&blend)
                .dynamic_state(&dynamic)
                .layout(self.pipeline_layout)
                .render_pass(self.render_pass)
                .subpass(0);
            let pipeline = d.create_graphics_pipelines(
                vk::PipelineCache::null(),
                std::slice::from_ref(&info),
                None,
            );
            d.destroy_shader_module(vert, None);
            d.destroy_shader_module(frag, None);
            self.pipeline = pipeline.map_err(|(_, e)| format!("stage pipeline: {e}"))?[0];
        }
        Ok(())
    }

    fn ensure_depth(&mut self, extent: vk::Extent2D) -> Result<vk::ImageView, String> {
        if let Some((image, current)) = &self.depth
            && *current == extent
        {
            return Ok(image.view);
        }
        self.wait_idle();
        self.destroy_targets();
        if let Some((image, _)) = self.depth.take() {
            image.destroy(&self.device);
        }
        let d = &self.device;
        let mut image = Image::null();
        let result = (|| unsafe {
            image.image = d
                .create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(self.depth_format)
                        .extent(vk::Extent3D {
                            width: extent.width,
                            height: extent.height,
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(vk::ImageUsageFlags::DEPTH_STENCIL_ATTACHMENT)
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    None,
                )
                .map_err(|e| format!("stage depth: {e}"))?;
            let req = d.get_image_memory_requirements(image.image);
            let index =
                self.find_memory(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
            image.memory = d
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(index),
                    None,
                )
                .map_err(|e| format!("stage depth memory: {e}"))?;
            d.bind_image_memory(image.image, image.memory, 0)
                .map_err(|e| format!("bind stage depth: {e}"))?;
            image.view = d
                .create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(image.image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(self.depth_format)
                        .subresource_range(vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::DEPTH,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 1,
                        }),
                    None,
                )
                .map_err(|e| format!("stage depth view: {e}"))?;
            Ok(())
        })();
        if let Err(e) = result {
            image.destroy(d);
            return Err(e);
        }
        let view = image.view;
        self.depth = Some((image, extent));
        Ok(view)
    }

    fn eye_targets(
        &mut self,
        images: &[vk::Image],
        image_index: usize,
        extent: vk::Extent2D,
        depth_view: vk::ImageView,
    ) -> Result<[vk::Framebuffer; 2], String> {
        if self.target_images != images {
            self.wait_idle();
            self.destroy_targets();
            self.target_images = images.to_vec();
            self.targets = (0..images.len()).map(|_| None).collect();
        }
        if self.commands.len() < images.len() {
            let d = &self.device;
            let bufs = unsafe {
                d.allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(self.command_pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count((images.len() - self.commands.len()) as u32),
                )
            }
            .map_err(|e| format!("stage commands: {e}"))?;
            for buf in bufs {
                let fence = unsafe {
                    d.create_fence(
                        &vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED),
                        None,
                    )
                }
                .map_err(|e| format!("stage fence: {e}"))?;
                self.commands.push((buf, fence));
            }
        }
        let image = *images
            .get(image_index)
            .ok_or("stage swapchain image index out of range")?;
        if self.targets[image_index].is_none() {
            let d = &self.device;
            let mut made: Vec<EyeTarget> = Vec::new();
            for layer in 0..2 {
                let view = unsafe {
                    d.create_image_view(
                        &vk::ImageViewCreateInfo::default()
                            .image(image)
                            .view_type(vk::ImageViewType::TYPE_2D)
                            .format(self.target_format)
                            .subresource_range(vk::ImageSubresourceRange {
                                aspect_mask: vk::ImageAspectFlags::COLOR,
                                base_mip_level: 0,
                                level_count: 1,
                                base_array_layer: layer,
                                layer_count: 1,
                            }),
                        None,
                    )
                };
                let view = match view {
                    Ok(view) => view,
                    Err(e) => {
                        made.into_iter().for_each(|t| t.destroy(d));
                        return Err(format!("stage eye view: {e}"));
                    }
                };
                let framebuffer = unsafe {
                    d.create_framebuffer(
                        &vk::FramebufferCreateInfo::default()
                            .render_pass(self.render_pass)
                            .attachments(&[view, depth_view])
                            .width(extent.width)
                            .height(extent.height)
                            .layers(1),
                        None,
                    )
                };
                match framebuffer {
                    Ok(framebuffer) => made.push(EyeTarget { view, framebuffer }),
                    Err(e) => {
                        unsafe { d.destroy_image_view(view, None) };
                        made.into_iter().for_each(|t| t.destroy(d));
                        return Err(format!("stage framebuffer: {e}"));
                    }
                }
            }
            self.targets[image_index] = Some(
                made.try_into()
                    .map_err(|_| "stage eye target count".to_string())?,
            );
        }
        let targets = self.targets[image_index].as_ref().unwrap();
        Ok([targets[0].framebuffer, targets[1].framebuffer])
    }

    /// Draws both eyes into array layers 0 and 1 of `images[image_index]`.
    pub fn render(
        &mut self,
        stage: &StageAsset,
        images: &[vk::Image],
        image_index: usize,
        views: &[xr::View; 2],
        extent: xr::Extent2Di,
    ) -> Result<(), String> {
        let extent = vk::Extent2D {
            width: u32::try_from(extent.width).map_err(|_| "negative stage extent")?,
            height: u32::try_from(extent.height).map_err(|_| "negative stage extent")?,
        };
        let depth_view = self.ensure_depth(extent)?;
        let framebuffers = self.eye_targets(images, image_index, extent, depth_view)?;
        let (cmd, fence) = self.commands[image_index];
        let d = &self.device;
        let model = stage.model_transform.to_cols_array();
        unsafe {
            d.wait_for_fences(&[fence], true, u64::MAX)
                .map_err(|e| format!("wait for stage frame: {e}"))?;
            d.reset_fences(&[fence])
                .map_err(|e| format!("reset stage fence: {e}"))?;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(|e| format!("begin stage frame: {e}"))?;
            let area = vk::Rect2D {
                offset: vk::Offset2D::default(),
                extent,
            };
            let clear = [
                vk::ClearValue {
                    color: vk::ClearColorValue {
                        float32: self.clear_color,
                    },
                },
                vk::ClearValue {
                    depth_stencil: vk::ClearDepthStencilValue {
                        depth: 1.0,
                        stencil: 0,
                    },
                },
            ];
            for (eye, view) in views.iter().enumerate() {
                d.cmd_begin_render_pass(
                    cmd,
                    &vk::RenderPassBeginInfo::default()
                        .render_pass(self.render_pass)
                        .framebuffer(framebuffers[eye])
                        .render_area(area)
                        .clear_values(&clear),
                    vk::SubpassContents::INLINE,
                );
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
                d.cmd_set_viewport(
                    cmd,
                    0,
                    &[vk::Viewport {
                        x: 0.0,
                        y: 0.0,
                        width: extent.width as f32,
                        height: extent.height as f32,
                        min_depth: 0.0,
                        max_depth: 1.0,
                    }],
                );
                d.cmd_set_scissor(cmd, 0, &[area]);
                d.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.pipeline_layout,
                    0,
                    &[self.descriptor_set],
                    &[],
                );
                d.cmd_bind_vertex_buffers(cmd, 0, &[self.vertices.buffer], &[0]);
                d.cmd_bind_index_buffer(cmd, self.indices.buffer, 0, vk::IndexType::UINT32);
                let push = PushBlock {
                    model,
                    view_projection: (projection_matrix(view.fov, NEAR, FAR) * view_matrix(view))
                        .to_cols_array(),
                };
                d.cmd_push_constants(
                    cmd,
                    self.pipeline_layout,
                    vk::ShaderStageFlags::VERTEX,
                    0,
                    as_bytes(std::slice::from_ref(&push)),
                );
                d.cmd_draw_indexed(cmd, self.index_count, 1, 0, 0, 0);
                d.cmd_end_render_pass(cmd);
            }
            d.end_command_buffer(cmd)
                .map_err(|e| format!("end stage frame: {e}"))?;
            d.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&[cmd])],
                fence,
            )
            .map_err(|e| format!("submit stage frame: {e}"))?;
        }
        Ok(())
    }

    fn wait_idle(&self) {
        let fences: Vec<_> = self.commands.iter().map(|(_, fence)| *fence).collect();
        if !fences.is_empty() {
            unsafe {
                let _ = self.device.wait_for_fences(&fences, true, u64::MAX);
            }
        }
    }

    fn destroy_targets(&mut self) {
        for targets in self.targets.drain(..).flatten() {
            for target in targets {
                target.destroy(&self.device);
            }
        }
        self.target_images.clear();
    }
}

impl Drop for StageRenderer {
    fn drop(&mut self) {
        self.wait_idle();
        self.destroy_targets();
        let d = &self.device;
        unsafe {
            if let Some((image, _)) = self.depth.take() {
                image.destroy(d);
            }
            for (_, fence) in self.commands.drain(..) {
                d.destroy_fence(fence, None);
            }
            d.destroy_command_pool(self.command_pool, None);
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.pipeline_layout, None);
            d.destroy_descriptor_pool(self.descriptor_pool, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
            d.destroy_sampler(self.sampler, None);
            d.destroy_render_pass(self.render_pass, None);
        }
        std::mem::replace(&mut self.texture, Image::null()).destroy(d);
        std::mem::replace(&mut self.vertices, Buffer::null()).destroy(d);
        std::mem::replace(&mut self.indices, Buffer::null()).destroy(d);
        std::mem::replace(&mut self.settings, Buffer::null()).destroy(d);
    }
}

impl Buffer {
    fn null() -> Self {
        Self {
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
        }
    }
    fn destroy(self, d: &ash::Device) {
        unsafe {
            d.destroy_buffer(self.buffer, None);
            d.free_memory(self.memory, None);
        }
    }
}

impl Image {
    fn null() -> Self {
        Self {
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
        }
    }
    fn destroy(self, d: &ash::Device) {
        unsafe {
            d.destroy_image_view(self.view, None);
            d.destroy_image(self.image, None);
            d.free_memory(self.memory, None);
        }
    }
}

impl EyeTarget {
    fn destroy(self, d: &ash::Device) {
        unsafe {
            d.destroy_framebuffer(self.framebuffer, None);
            d.destroy_image_view(self.view, None);
        }
    }
}

fn as_bytes<T: Copy>(slice: &[T]) -> &[u8] {
    // SAFETY: T is plain old data (vertices, indices and repr(C) float blocks).
    unsafe { std::slice::from_raw_parts(slice.as_ptr().cast(), std::mem::size_of_val(slice)) }
}

#[cfg(all(test, target_os = "macos"))]
mod native_stage_tests {
    use super::*;
    use crate::stage::StageSettings;

    const SIZE: u32 = 64;

    struct Gpu {
        // Dropping the entry unloads the driver, so it lives as long as the device.
        _entry: ash::Entry,
        instance: ash::Instance,
        physical: vk::PhysicalDevice,
        device: ash::Device,
        queue: vk::Queue,
        family: u32,
    }

    fn gpu() -> Gpu {
        use super::super::vulkan::portability;
        let entry = portability::load_entry();
        let mut extensions = Vec::new();
        let mut flags = vk::InstanceCreateFlags::empty();
        portability::add_instance_enumeration(&entry, &mut extensions, &mut flags);
        let ptrs: Vec<_> = extensions.iter().map(|n| n.as_ptr()).collect();
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .flags(flags)
                    .application_info(
                        &vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_2),
                    )
                    .enabled_extension_names(&ptrs),
                None,
            )
        }
        .unwrap();
        let physical = unsafe { instance.enumerate_physical_devices().unwrap()[0] };
        let family = unsafe { instance.get_physical_device_queue_family_properties(physical) }
            .iter()
            .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .unwrap() as u32;
        let mut extensions = Vec::new();
        portability::add_device_subset(&instance, physical, &mut extensions);
        let ptrs: Vec<_> = extensions.iter().map(|n| n.as_ptr()).collect();
        let device = unsafe {
            instance.create_device(
                physical,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&[vk::DeviceQueueCreateInfo::default()
                        .queue_family_index(family)
                        .queue_priorities(&[1.0])])
                    .enabled_extension_names(&ptrs),
                None,
            )
        }
        .unwrap();
        let queue = unsafe { device.get_device_queue(family, 0) };
        Gpu {
            _entry: entry,
            instance,
            physical,
            device,
            queue,
            family,
        }
    }

    fn view() -> xr::View {
        xr::View {
            pose: xr::Posef::IDENTITY,
            fov: xr::Fovf {
                angle_left: -std::f32::consts::FRAC_PI_4,
                angle_right: std::f32::consts::FRAC_PI_4,
                angle_up: std::f32::consts::FRAC_PI_4,
                angle_down: -std::f32::consts::FRAC_PI_4,
            },
        }
    }

    /// One triangle drawn clockwise as the viewer sees it (front-facing) above
    /// the view centre, and one anticlockwise (back-facing) below it, both 2 m
    /// in front of a viewer at the origin looking down -Z.
    fn stage(rgba: [u8; 4], luminance: bool) -> StageAsset {
        let v = |x: f32, y: f32| StageVertex {
            position: [x, y, -2.0],
            tex_coord: [0.5, 0.5],
        };
        StageAsset {
            id: NEXT_TEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            source_path: "native-stage-test".into(),
            vertices: vec![
                v(0.0, 1.8),
                v(1.5, 0.2),
                v(-1.5, 0.2),
                v(0.0, -1.8),
                v(1.5, -0.2),
                v(-1.5, -0.2),
            ],
            indices: vec![0, 1, 2, 3, 4, 5],
            texture_rgba: rgba.repeat(4),
            texture_width: 2,
            texture_height: 2,
            texture_luminance: luminance,
            model_transform: glam::Mat4::IDENTITY,
            settings: StageSettings {
                primary_color: [1.0; 4],
                secondary_color: [0.2, 0.0, 0.0, 1.0],
                backface_culling: true,
                greyscale: luminance,
                ..StageSettings::default()
            },
        }
    }

    static NEXT_TEST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1000);

    /// Renders into a two-layer sRGB target and returns both layers as RGBA8.
    fn render(gpu: &Gpu, asset: &StageAsset) -> Vec<u8> {
        let d = &gpu.device;
        let ctx = StageDevice {
            instance: &gpu.instance,
            physical_device: gpu.physical,
            device: d,
            queue: gpu.queue,
            queue_family_index: gpu.family,
        };
        let format = vk::Format::R8G8B8A8_SRGB;
        let mut renderer = StageRenderer::new(&ctx, asset, format).unwrap();
        let target = unsafe {
            d.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D {
                        width: SIZE,
                        height: SIZE,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(2)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(
                        vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
                    ),
                None,
            )
        }
        .unwrap();
        let req = unsafe { d.get_image_memory_requirements(target) };
        let memory = unsafe {
            d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(req.size)
                    .memory_type_index(
                        renderer
                            .find_memory(
                                req.memory_type_bits,
                                vk::MemoryPropertyFlags::DEVICE_LOCAL,
                            )
                            .unwrap(),
                    ),
                None,
            )
        }
        .unwrap();
        unsafe { d.bind_image_memory(target, memory, 0) }.unwrap();

        let extent = xr::Extent2Di {
            width: SIZE as i32,
            height: SIZE as i32,
        };
        renderer
            .render(asset, &[target], 0, &[view(), view()], extent)
            .unwrap();

        let bytes = (SIZE * SIZE * 4 * 2) as usize;
        let readback = renderer
            .host_buffer(vk::BufferUsageFlags::TRANSFER_DST, &vec![0; bytes])
            .unwrap();
        renderer
            .one_time_commands(|cmd| unsafe {
                d.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[vk::ImageMemoryBarrier {
                        src_access_mask: vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
                        dst_access_mask: vk::AccessFlags::TRANSFER_READ,
                        old_layout: vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
                        new_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                        src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                        dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
                        image: target,
                        subresource_range: vk::ImageSubresourceRange {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            base_mip_level: 0,
                            level_count: 1,
                            base_array_layer: 0,
                            layer_count: 2,
                        },
                        ..Default::default()
                    }],
                );
                d.cmd_copy_image_to_buffer(
                    cmd,
                    target,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    readback.buffer,
                    &[vk::BufferImageCopy {
                        image_subresource: vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 2,
                        },
                        image_extent: vk::Extent3D {
                            width: SIZE,
                            height: SIZE,
                            depth: 1,
                        },
                        ..Default::default()
                    }],
                );
            })
            .unwrap();
        let mut pixels = vec![0u8; bytes];
        unsafe {
            let ptr = d
                .map_memory(
                    readback.memory,
                    0,
                    bytes as u64,
                    vk::MemoryMapFlags::empty(),
                )
                .unwrap();
            std::ptr::copy_nonoverlapping(ptr.cast::<u8>(), pixels.as_mut_ptr(), bytes);
            d.unmap_memory(readback.memory);
        }
        readback.destroy(d);
        drop(renderer);
        unsafe {
            d.destroy_image(target, None);
            d.free_memory(memory, None);
        }
        pixels
    }

    fn pixel(pixels: &[u8], layer: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((layer * SIZE * SIZE + y * SIZE + x) * 4) as usize;
        pixels[i..i + 4].try_into().unwrap()
    }

    fn close(actual: [u8; 4], expected: [u8; 3]) -> bool {
        actual[..3]
            .iter()
            .zip(expected)
            .all(|(a, e)| (*a as i32 - e as i32).abs() <= 3)
    }

    /// Renders on the real driver and reads pixels back: background colour,
    /// orientation (Y flip), back-face culling and both texture paths.
    #[test]
    #[ignore = "requires an explicit native Vulkan ICD and Metal access"]
    fn native_stage_render() {
        let gpu = gpu();
        // Secondary (0.2, 0, 0) linear is about 124 in sRGB.
        let background = [124, 0, 0];
        for (rgba, luminance, expected) in [
            // BC4-style luminance: 128 sRGB round-trips to 128 through the decode.
            ([128, 128, 128, 255], true, [128, 128, 128]),
            ([0, 0, 255, 255], false, [0, 0, 255]),
        ] {
            let pixels = render(&gpu, &stage(rgba, luminance));
            for layer in 0..2 {
                let top = pixel(&pixels, layer, SIZE / 2, SIZE / 4);
                let bottom = pixel(&pixels, layer, SIZE / 2, SIZE * 3 / 4);
                let corner = pixel(&pixels, layer, 1, SIZE / 2);
                eprintln!(
                    "luminance={luminance} layer={layer}: top={top:?} bottom={bottom:?} corner={corner:?}"
                );
                assert!(close(top, expected), "front triangle above centre: {top:?}");
                assert!(
                    close(bottom, background),
                    "back-facing triangle culled: {bottom:?}"
                );
                assert!(
                    close(corner, background),
                    "cleared to secondary: {corner:?}"
                );
            }
        }
        // Without culling the lower, back-facing triangle is drawn.
        let mut unculled = stage([0, 0, 255, 255], false);
        unculled.settings.backface_culling = false;
        let pixels = render(&gpu, &unculled);
        let bottom = pixel(&pixels, 0, SIZE / 2, SIZE * 3 / 4);
        assert!(
            close(bottom, [0, 0, 255]),
            "unculled lower triangle: {bottom:?}"
        );
        unsafe {
            gpu.device.destroy_device(None);
            gpu.instance.destroy_instance(None);
        }
    }
}
