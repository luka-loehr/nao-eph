//! Triangle meshes for X3D geometry nodes, in Webots' Z-up convention.

use glam::{Vec2, Vec3};
use std::f32::consts::{PI, TAU};

#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
}

#[derive(Clone, Debug, Default)]
pub struct Mesh {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

impl Mesh {
    fn push(&mut self, position: Vec3, normal: Vec3, uv: Vec2) -> u32 {
        self.vertices.push(Vertex {
            position: position.to_array(),
            normal: normal.normalize_or_zero().to_array(),
            uv: uv.to_array(),
        });
        self.vertices.len() as u32 - 1
    }

    fn quad(&mut self, corners: [Vec3; 4], normal: Vec3) {
        let uvs = [
            Vec2::new(0.0, 1.0),
            Vec2::new(1.0, 1.0),
            Vec2::new(1.0, 0.0),
            Vec2::new(0.0, 0.0),
        ];
        let base = self.vertices.len() as u32;
        for (corner, uv) in corners.into_iter().zip(uvs) {
            self.push(corner, normal, uv);
        }
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
}

pub fn box_mesh(size: Vec3) -> Mesh {
    let h = size * 0.5;
    let mut mesh = Mesh::default();
    let faces = [
        (Vec3::X, Vec3::Y, Vec3::Z),
        (Vec3::NEG_X, Vec3::NEG_Y, Vec3::Z),
        (Vec3::Y, Vec3::NEG_X, Vec3::Z),
        (Vec3::NEG_Y, Vec3::X, Vec3::Z),
        (Vec3::Z, Vec3::X, Vec3::Y),
        (Vec3::NEG_Z, Vec3::X, Vec3::NEG_Y),
    ];
    for (n, u, v) in faces {
        let c = n * h;
        let (du, dv) = (u * h, v * h);
        mesh.quad([c - du - dv, c + du - dv, c + du + dv, c - du + dv], n);
    }
    mesh
}

pub fn plane_mesh(size: Vec2) -> Mesh {
    let (x, y) = (size.x * 0.5, size.y * 0.5);
    let mut mesh = Mesh::default();
    mesh.quad(
        [
            Vec3::new(-x, -y, 0.0),
            Vec3::new(x, -y, 0.0),
            Vec3::new(x, y, 0.0),
            Vec3::new(-x, y, 0.0),
        ],
        Vec3::Z,
    );
    mesh
}

pub fn sphere_mesh(radius: f32, subdivision: u32) -> Mesh {
    let rings = (8 * subdivision.max(1)).clamp(8, 48);
    let segments = rings * 2;
    let mut mesh = Mesh::default();
    for r in 0..=rings {
        let theta = PI * r as f32 / rings as f32;
        for s in 0..=segments {
            let phi = TAU * s as f32 / segments as f32;
            let n = Vec3::new(theta.sin() * phi.cos(), theta.sin() * phi.sin(), theta.cos());
            mesh.push(
                n * radius,
                n,
                Vec2::new(s as f32 / segments as f32, r as f32 / rings as f32),
            );
        }
    }
    let stride = segments + 1;
    for r in 0..rings {
        for s in 0..segments {
            let a = r * stride + s;
            let b = a + stride;
            mesh.indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    mesh
}

/// Cylinder along Z; with `capsule`, the ends are hemispheres.
fn tube_mesh(height: f32, radius: f32, side: bool, top: bool, bottom: bool, capsule: bool) -> Mesh {
    const SEGMENTS: u32 = 32;
    let mut mesh = Mesh::default();
    let half = height * 0.5;
    if side {
        let base = mesh.vertices.len() as u32;
        for s in 0..=SEGMENTS {
            let phi = TAU * s as f32 / SEGMENTS as f32;
            let n = Vec3::new(phi.cos(), phi.sin(), 0.0);
            let u = s as f32 / SEGMENTS as f32;
            mesh.push(n * radius - Vec3::Z * half, n, Vec2::new(u, 1.0));
            mesh.push(n * radius + Vec3::Z * half, n, Vec2::new(u, 0.0));
        }
        for s in 0..SEGMENTS {
            let a = base + s * 2;
            mesh.indices.extend_from_slice(&[a, a + 2, a + 1, a + 1, a + 2, a + 3]);
        }
    }
    for (enabled, sign) in [(top, 1.0f32), (bottom, -1.0f32)] {
        if !enabled {
            continue;
        }
        if capsule {
            const RINGS: u32 = 12;
            let base = mesh.vertices.len() as u32;
            for r in 0..=RINGS {
                let theta = (PI * 0.5) * r as f32 / RINGS as f32;
                for s in 0..=SEGMENTS {
                    let phi = TAU * s as f32 / SEGMENTS as f32;
                    let n = Vec3::new(
                        theta.cos() * phi.cos(),
                        theta.cos() * phi.sin(),
                        sign * theta.sin(),
                    );
                    mesh.push(n * radius + Vec3::Z * sign * half, n, Vec2::ZERO);
                }
            }
            let stride = SEGMENTS + 1;
            for r in 0..RINGS {
                for s in 0..SEGMENTS {
                    let a = base + r * stride + s;
                    let b = a + stride;
                    mesh.indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
                }
            }
        } else {
            let n = Vec3::Z * sign;
            let center = mesh.push(n * half, n, Vec2::splat(0.5));
            for s in 0..=SEGMENTS {
                let phi = TAU * s as f32 / SEGMENTS as f32;
                let (c, si) = (phi.cos(), phi.sin());
                mesh.push(
                    Vec3::new(c * radius, si * radius, sign * half),
                    n,
                    Vec2::new(0.5 + c * 0.5, 0.5 + si * 0.5),
                );
            }
            for s in 0..SEGMENTS {
                mesh.indices.extend_from_slice(&[center, center + 1 + s, center + 2 + s]);
            }
        }
    }
    mesh
}

pub fn cylinder_mesh(height: f32, radius: f32, side: bool, top: bool, bottom: bool) -> Mesh {
    tube_mesh(height, radius, side, top, bottom, false)
}

pub fn capsule_mesh(height: f32, radius: f32, side: bool, top: bool, bottom: bool) -> Mesh {
    tube_mesh(height, radius, side, top, bottom, true)
}

/// Triangulates an IndexedFaceSet and computes normals that are smooth within `crease_angle`.
pub fn indexed_face_set(
    points: &[Vec3],
    coord_index: &[i64],
    tex_points: &[Vec2],
    tex_index: &[i64],
    crease_angle: f32,
    ccw: bool,
) -> Mesh {
    // Split polygons at -1 and fan-triangulate them.
    let mut triangles: Vec<[(usize, usize); 3]> = Vec::new();
    let mut start = 0;
    let tex_for = |i: usize| -> usize {
        if !tex_index.is_empty() {
            tex_index.get(i).copied().unwrap_or(0).max(0) as usize
        } else {
            coord_index[i].max(0) as usize
        }
    };
    for i in 0..=coord_index.len() {
        if i == coord_index.len() || coord_index[i] < 0 {
            let poly: Vec<usize> = (start..i).collect();
            for k in 1..poly.len().saturating_sub(1) {
                let corner = |j: usize| (coord_index[poly[j]] as usize, tex_for(poly[j]));
                let (a, b, c) = (corner(0), corner(k), corner(k + 1));
                triangles.push(if ccw { [a, b, c] } else { [a, c, b] });
            }
            start = i + 1;
        }
    }
    triangles.retain(|t| t.iter().all(|(p, _)| *p < points.len()));

    // Without texture coordinates, Webots maps S along the longest bounding-box axis and T along
    // the second longest, keeping the texture's aspect ratio.
    let generated: Vec<Vec2>;
    let tex_points = if tex_points.is_empty() && !points.is_empty() {
        let (min, max) = points
            .iter()
            .fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), p| (lo.min(*p), hi.max(*p)));
        let size = (max - min).to_array();
        let mut axes = [0usize, 1, 2];
        axes.sort_by(|a, b| size[*b].total_cmp(&size[*a]));
        let (s, t) = (axes[0], axes[1]);
        let extent = size[s].max(1e-6);
        generated = points
            .iter()
            .map(|p| {
                let p = (*p - min).to_array();
                Vec2::new(p[s] / extent, p[t] / extent)
            })
            .collect();
        &generated[..]
    } else {
        tex_points
    };

    let face_normals: Vec<Vec3> = triangles
        .iter()
        .map(|t| {
            let (a, b, c) = (points[t[0].0], points[t[1].0], points[t[2].0]);
            (b - a).cross(c - a)
        })
        .collect();
    let mut faces_of_point: Vec<Vec<usize>> = vec![Vec::new(); points.len()];
    for (f, t) in triangles.iter().enumerate() {
        for (p, _) in t {
            faces_of_point[*p].push(f);
        }
    }
    let cos_crease = crease_angle.cos();
    let mut mesh = Mesh::default();
    for (f, t) in triangles.iter().enumerate() {
        let own = face_normals[f].normalize_or_zero();
        for (p, tex) in t {
            let mut n = Vec3::ZERO;
            if crease_angle > 0.0 {
                for &g in &faces_of_point[*p] {
                    if face_normals[g].normalize_or_zero().dot(own) >= cos_crease {
                        n += face_normals[g];
                    }
                }
            }
            if n.length_squared() == 0.0 {
                n = own;
            }
            let uv = tex_points.get(*tex).copied().unwrap_or(Vec2::ZERO);
            let idx = mesh.push(points[*p], n, Vec2::new(uv.x, 1.0 - uv.y));
            mesh.indices.push(idx);
        }
    }
    mesh
}
