use glam::{Mat4, Vec3, Vec4};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_STAGE_ID: AtomicU64 = AtomicU64::new(1);

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct StageVertex {
    pub position: [f32; 3],
    pub tex_coord: [f32; 2],
}

#[derive(Clone, Copy, Debug)]
pub struct StageSettings {
    pub primary_color: [f32; 4],
    pub secondary_color: [f32; 4],
    pub vignette_inner_radius: f32,
    pub vignette_outer_radius: f32,
    pub fresnel_strength: f32,
    pub backface_culling: bool,
    pub greyscale: bool,
    pub wireframe: bool,
}

impl Default for StageSettings {
    fn default() -> Self {
        Self {
            primary_color: [1.0; 4],
            secondary_color: [1.0; 4],
            vignette_inner_radius: 0.0,
            vignette_outer_radius: 0.0,
            fresnel_strength: 0.0,
            backface_culling: false,
            greyscale: false,
            wireframe: false,
        }
    }
}

pub struct StageAsset {
    pub id: u64,
    pub source_path: PathBuf,
    pub vertices: Vec<StageVertex>,
    pub indices: Vec<u32>,
    pub texture_rgba: Vec<u8>,
    pub texture_width: u32,
    pub texture_height: u32,
    pub model_transform: Mat4,
    pub settings: StageSettings,
}

impl StageAsset {
    pub fn load(
        path: impl AsRef<Path>,
        model_transform: Mat4,
        settings: StageSettings,
    ) -> Result<Self, String> {
        let path = path.as_ref();
        let load_options = tobj::LoadOptions {
            triangulate: true,
            single_index: true,
            ..Default::default()
        };
        let (models, materials) =
            tobj::load_obj(path, &load_options).map_err(|e| format!("OBJ load failed: {e}"))?;
        if models.is_empty() {
            return Err("OBJ contains no models".into());
        }

        let materials = materials.map_err(|e| format!("MTL load failed: {e}"))?;
        let mut vertices = Vec::new();
        let mut indices = Vec::new();

        for model in &models {
            let mesh = &model.mesh;
            let base = vertices.len() as u32;
            let position_count = mesh.positions.len() / 3;
            if position_count == 0 {
                continue;
            }

            for i in 0..position_count {
                let position = [
                    mesh.positions[i * 3],
                    mesh.positions[i * 3 + 1],
                    mesh.positions[i * 3 + 2],
                ];
                let tex_coord = if mesh.texcoords.len() >= (i + 1) * 2 {
                    [mesh.texcoords[i * 2], 1.0 - mesh.texcoords[i * 2 + 1]]
                } else {
                    [0.0, 0.0]
                };
                vertices.push(StageVertex {
                    position,
                    tex_coord,
                });
            }
            indices.extend(mesh.indices.iter().map(|i| base + *i));
        }

        if vertices.is_empty() || indices.is_empty() {
            return Err("OBJ contains no triangles".into());
        }
        if vertices.len() > 65_000 {
            return Err(format!(
                "stage OBJ has {} vertices; SteamVR stage overrides are limited to 65,000",
                vertices.len()
            ));
        }

        let texture_name = models
            .iter()
            .filter_map(|model| model.mesh.material_id)
            .filter_map(|id| materials.get(id))
            .find_map(|material| material.diffuse_texture.clone())
            .or_else(|| {
                materials
                    .iter()
                    .find_map(|material| material.diffuse_texture.clone())
            });

        let (texture_rgba, texture_width, texture_height) =
            if let Some(texture_name) = texture_name {
                let texture_path = path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(texture_name);

                // Alyx's diorama MTLs name PNG textures, while current game depots
                // ship the corresponding baked textures as DDS files. SteamVR accepts
                // those stage assets transparently, so mirror that behaviour here.
                let mut candidates = vec![texture_path.clone()];
                for extension in ["dds", "png", "tga"] {
                    let candidate = texture_path.with_extension(extension);
                    if !candidates.contains(&candidate) {
                        candidates.push(candidate);
                    }
                }

                let mut errors = Vec::new();
                let mut loaded = None;
                for candidate in candidates {
                    match image::open(&candidate) {
                        Ok(image) => {
                            if candidate != texture_path {
                                log::info!(
                                    "stage texture {:?} is unavailable; using {:?}",
                                    texture_path,
                                    candidate
                                );
                            }
                            loaded = Some((candidate, image));
                            break;
                        }
                        Err(error) => errors.push(format!("{candidate:?}: {error}")),
                    }
                }

                let (loaded_path, image) = loaded.ok_or_else(|| {
                    format!(
                        "stage texture {:?} failed to load; tried {}",
                        texture_path,
                        errors.join("; ")
                    )
                })?;
                let image = image.to_rgba8();
                let (width, height) = image.dimensions();
                log::info!(
                    "loaded stage texture {:?}: {}x{} RGBA",
                    loaded_path,
                    width,
                    height
                );
                (image.into_raw(), width, height)
            } else {
                log::warn!("stage OBJ {:?} has no diffuse texture; using white", path);
                (vec![255, 255, 255, 255], 1, 1)
            };

        Ok(Self {
            id: NEXT_STAGE_ID.fetch_add(1, Ordering::Relaxed),
            source_path: path.to_owned(),
            vertices,
            indices,
            texture_rgba,
            texture_width,
            texture_height,
            model_transform,
            settings,
        })
    }
}

pub fn mat4_from_hmd34(matrix: &openvr::HmdMatrix34_t) -> Mat4 {
    Mat4::from_cols(
        Vec4::new(matrix.m[0][0], matrix.m[1][0], matrix.m[2][0], 0.0),
        Vec4::new(matrix.m[0][1], matrix.m[1][1], matrix.m[2][1], 0.0),
        Vec4::new(matrix.m[0][2], matrix.m[1][2], matrix.m[2][2], 0.0),
        Vec4::new(matrix.m[0][3], matrix.m[1][3], matrix.m[2][3], 1.0),
    )
}

pub fn view_matrix(view: &openxr::View) -> Mat4 {
    let q = view.pose.orientation;
    let p = view.pose.position;
    Mat4::from_rotation_translation(
        glam::Quat::from_xyzw(q.x, q.y, q.z, q.w),
        Vec3::new(p.x, p.y, p.z),
    )
    .inverse()
}

pub fn projection_matrix(fov: openxr::Fovf, near: f32, far: f32) -> Mat4 {
    let tan_left = fov.angle_left.tan();
    let tan_right = fov.angle_right.tan();
    let tan_down = fov.angle_down.tan();
    let tan_up = fov.angle_up.tan();

    let width = tan_right - tan_left;
    let height = tan_up - tan_down;

    let m00 = 2.0 / width;
    let m11 = 2.0 / height;
    let m20 = (tan_right + tan_left) / width;
    let m21 = (tan_up + tan_down) / height;
    let m22 = far / (near - far);
    let m32 = (far * near) / (near - far);

    Mat4::from_cols(
        Vec4::new(m00, 0.0, 0.0, 0.0),
        Vec4::new(0.0, m11, 0.0, 0.0),
        Vec4::new(m20, m21, m22, -1.0),
        Vec4::new(0.0, 0.0, m32, 0.0),
    )
}
