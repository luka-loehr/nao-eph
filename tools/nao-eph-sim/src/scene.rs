//! Scene graph built from the X3D world that Webots streams, plus live pose updates.

use crate::mesh::{self, Mesh};
use anyhow::{Context, Result};
use glam::{Mat4, Quat, Vec2, Vec3};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct Material {
    pub base_color: Vec3,
    pub roughness: f32,
    pub metalness: f32,
    pub emissive: Vec3,
    pub transparency: f32,
    pub base_color_map: Option<String>,
    pub texture_scale: Vec2,
}

impl Default for Material {
    // Webots' PBRAppearance defaults.
    fn default() -> Self {
        Self {
            base_color: Vec3::ONE,
            roughness: 0.0,
            metalness: 1.0,
            emissive: Vec3::ZERO,
            transparency: 0.0,
            base_color_map: None,
            texture_scale: Vec2::ONE,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Node {
    pub translation: Vec3,
    pub rotation: Quat,
    pub scale: Vec3,
    pub children: Vec<usize>,
    /// (mesh index, material index)
    pub shapes: Vec<(usize, usize)>,
    /// Walls and windows, which the viewer hides when they stand between camera and room.
    pub cutaway: bool,
    /// Roofs, which the viewer hides when the camera is close to the house.
    pub roof: bool,
}

impl Node {
    fn new() -> Self {
        Self {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            children: Vec::new(),
            shapes: Vec::new(),
            cutaway: false,
            roof: false,
        }
    }

    pub fn local(&self) -> Mat4 {
        Mat4::from_scale_rotation_translation(self.scale, self.rotation, self.translation)
    }
}

#[derive(Clone, Debug)]
pub struct Robot {
    pub node: usize,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct PointLight {
    /// Node the light is attached to; `location` is in its frame.
    pub node: usize,
    pub location: Vec3,
    pub color: Vec3,
    pub intensity: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub nodes: Vec<Node>,
    pub root: usize,
    pub meshes: Vec<Mesh>,
    pub materials: Vec<Material>,
    pub viewpoint: Option<(Vec3, Quat)>,
    pub robots: Vec<Robot>,
    pub point_lights: Vec<PointLight>,
    /// Skybox faces: right, left, top, bottom, front, back.
    pub sky: Option<[String; 6]>,
    by_id: HashMap<String, usize>,
}

/// One instance to draw: world matrix plus mesh and material.
pub struct Draw {
    pub world: Mat4,
    pub mesh: usize,
    pub material: usize,
    pub cutaway: bool,
    pub roof: bool,
}

#[derive(Default)]
struct Parser<'a, 'input> {
    /// Every defining element by id. Webots can stream a `USE` before its definition (the right
    /// hand reuses the left hand's meshes but comes first), so references resolve through this.
    defs: HashMap<&'a str, roxmltree::Node<'a, 'input>>,
    scene: Scene,
    meshes_by_id: HashMap<String, usize>,
    materials_by_id: HashMap<String, usize>,
    shapes_by_id: HashMap<String, (usize, usize)>,
    coords_by_id: HashMap<String, Vec<Vec3>>,
}

impl Scene {
    pub fn parse(x3d: &str) -> Result<Self> {
        let doc = roxmltree::Document::parse(x3d).context("parsing X3D")?;
        let scene_el = doc
            .descendants()
            .find(|n| n.has_tag_name("Scene"))
            .context("X3D has no <Scene>")?;
        let mut parser = Parser::default();
        for node in doc.descendants().filter(|n| n.is_element() && n.attribute("USE").is_none()) {
            if let Some(id) = node.attribute("id") {
                parser.defs.insert(id, node);
            }
        }
        parser.scene.nodes.push(Node::new());
        parser.scene.root = 0;
        for child in scene_el.children().filter(|c| c.is_element()) {
            parser.element(child, 0);
        }
        Ok(parser.scene)
    }

    /// Applies one entry of an `application/json` update (`id` is the bare number).
    pub fn apply_update(&mut self, id: u64, translation: Option<Vec3>, rotation: Option<Quat>) {
        if let Some(&node) = self.by_id.get(&format!("n{id}")) {
            let node = &mut self.nodes[node];
            if let Some(t) = translation {
                node.translation = t;
            }
            if let Some(r) = rotation {
                node.rotation = r;
            }
        }
    }

    pub fn world_matrix_of(&self, target: usize) -> Option<Mat4> {
        fn walk(scene: &Scene, node: usize, parent: Mat4, target: usize) -> Option<Mat4> {
            let world = parent * scene.nodes[node].local();
            if node == target {
                return Some(world);
            }
            scene.nodes[node]
                .children
                .iter()
                .find_map(|&c| walk(scene, c, world, target))
        }
        walk(self, self.root, Mat4::IDENTITY, target)
    }

    /// Only what hangs below `node`, e.g. one robot for the studio view.
    pub fn draws_under(&self, node: usize) -> Vec<Draw> {
        let Some(world) = self.world_matrix_of(node) else { return Vec::new() };
        let parent = world * self.nodes[node].local().inverse();
        self.draws_from(node, parent)
    }

    pub fn draws(&self) -> Vec<Draw> {
        self.draws_from(self.root, Mat4::IDENTITY)
    }

    fn draws_from(&self, start: usize, parent: Mat4) -> Vec<Draw> {
        let mut out = Vec::new();
        let mut stack = vec![(start, parent, 0u32, false, false)];
        while let Some((node, parent, depth, cutaway, roof)) = stack.pop() {
            // Guards against USE cycles.
            if depth > 64 {
                continue;
            }
            let n = &self.nodes[node];
            let world = parent * n.local();
            let cutaway = cutaway || n.cutaway;
            let roof = roof || n.roof;
            for &(mesh, material) in &n.shapes {
                out.push(Draw { world, mesh, material, cutaway, roof });
            }
            for &child in &n.children {
                stack.push((child, world, depth + 1, cutaway, roof));
            }
        }
        out
    }
}

fn attr<'a>(el: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    el.attribute(name)
}

/// The id a `USE` refers to. Webots writes most references as `n123` but some as a bare `123`.
fn use_ref(el: roxmltree::Node) -> Option<String> {
    el.attribute("USE").map(|u| {
        if u.starts_with('n') { u.to_string() } else { format!("n{u}") }
    })
}

fn floats(s: &str) -> Vec<f32> {
    s.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .collect()
}

fn vec3(s: Option<&str>, default: Vec3) -> Vec3 {
    s.map(floats)
        .filter(|v| v.len() >= 3)
        .map(|v| Vec3::new(v[0], v[1], v[2]))
        .unwrap_or(default)
}

pub fn axis_angle(s: &str) -> Option<Quat> {
    let v = floats(s);
    if v.len() < 4 {
        return None;
    }
    let axis = Vec3::new(v[0], v[1], v[2]);
    if axis.length_squared() < 1e-12 {
        return Some(Quat::IDENTITY);
    }
    Some(Quat::from_axis_angle(axis.normalize(), v[3]))
}

fn flag(el: roxmltree::Node, name: &str) -> bool {
    !matches!(attr(el, name), Some("FALSE") | Some("false"))
}

impl<'a, 'input> Parser<'a, 'input> {
    fn element(&mut self, el: roxmltree::Node<'a, 'input>, parent: usize) {
        let tag = el.tag_name().name();
        if attr(el, "role") == Some("boundingObject") {
            return;
        }
        match tag {
            "Viewpoint" => {
                let position = vec3(attr(el, "position"), Vec3::new(0.0, 0.0, 1.0));
                let orientation = attr(el, "orientation")
                    .and_then(axis_angle)
                    .unwrap_or(Quat::IDENTITY);
                self.scene.viewpoint = Some((position, orientation));
            }
            "PointLight" => self.scene.point_lights.push(PointLight {
                node: parent,
                location: vec3(attr(el, "location"), Vec3::ZERO),
                color: vec3(attr(el, "color"), Vec3::ONE),
                intensity: attr(el, "intensity").and_then(|v| v.parse().ok()).unwrap_or(1.0),
            }),
            "Background" => {
                let url = |name: &str| attr(el, name).map(|u| u.trim_matches('"').to_string());
                let faces = ["rightUrl", "leftUrl", "topUrl", "bottomUrl", "frontUrl", "backUrl"].map(url);
                if faces.iter().all(Option::is_some) {
                    self.scene.sky = Some(faces.map(Option::unwrap));
                }
            }
            "WorldInfo" | "DirectionalLight" | "SpotLight" | "Fog" | "head" | "meta" => {}
            "Shape" => {
                if let Some(shape) = self.shape(el) {
                    self.scene.nodes[parent].shapes.push(shape);
                }
            }
            _ => {
                if let Some(used) = use_ref(el) {
                    if let Some(&node) = self.scene.by_id.get(&used) {
                        self.scene.nodes[parent].children.push(node);
                    } else if let Some(&def) = self.defs.get(used.as_str()) {
                        self.element(def, parent);
                    }
                    return;
                }
                // Already built because a USE came first: just attach it here too.
                if let Some(&node) = attr(el, "id").and_then(|id| self.scene.by_id.get(id)) {
                    self.scene.nodes[parent].children.push(node);
                    return;
                }
                let mut node = Node::new();
                node.translation = vec3(attr(el, "translation"), Vec3::ZERO);
                node.rotation = attr(el, "rotation").and_then(axis_angle).unwrap_or(Quat::IDENTITY);
                node.scale = vec3(attr(el, "scale"), Vec3::ONE);
                let name = attr(el, "name").unwrap_or("").to_lowercase();
                node.cutaway = ["wall", "window", "door", "radiator"].iter().any(|k| name.contains(k));
                node.roof = name.contains("roof");
                let index = self.scene.nodes.len();
                self.scene.nodes.push(node);
                self.scene.nodes[parent].children.push(index);
                if let Some(id) = attr(el, "id") {
                    self.scene.by_id.insert(id.to_string(), index);
                }
                if tag == "Robot" {
                    self.scene.robots.push(Robot {
                        node: index,
                        name: attr(el, "name").unwrap_or("robot").to_string(),
                    });
                }
                for child in el.children().filter(|c| c.is_element()) {
                    self.element(child, index);
                }
            }
        }
    }

    fn shape(&mut self, el: roxmltree::Node<'a, 'input>) -> Option<(usize, usize)> {
        if let Some(used) = use_ref(el) {
            if let Some(&shape) = self.shapes_by_id.get(&used) {
                return Some(shape);
            }
            return self.defs.get(used.as_str()).copied().and_then(|def| self.shape(def));
        }
        let mut material = None;
        let mut mesh = None;
        for child in el.children().filter(|c| c.is_element()) {
            match child.tag_name().name() {
                "PBRAppearance" | "Appearance" => material = Some(self.material(child)),
                _ => mesh = mesh.or_else(|| self.geometry(child)),
            }
        }
        let material = material.unwrap_or_else(|| {
            self.scene.materials.push(Material::default());
            self.scene.materials.len() - 1
        });
        let shape = (mesh?, material);
        if let Some(id) = attr(el, "id") {
            self.shapes_by_id.insert(id.to_string(), shape);
        }
        Some(shape)
    }

    fn material(&mut self, el: roxmltree::Node<'a, 'input>) -> usize {
        if let Some(used) = use_ref(el) {
            if let Some(&m) = self.materials_by_id.get(&used) {
                return m;
            }
            if let Some(&def) = self.defs.get(used.as_str()) {
                return self.material(def);
            }
        }
        let mut m = Material::default();
        if el.has_tag_name("Appearance") {
            // Legacy Phong appearance: map onto a dielectric PBR material.
            m.metalness = 0.0;
            m.roughness = 0.6;
            if let Some(mat) = el.children().find(|c| c.has_tag_name("Material")) {
                m.base_color = vec3(attr(mat, "diffuseColor"), Vec3::splat(0.8));
                m.emissive = vec3(attr(mat, "emissiveColor"), Vec3::ZERO);
                m.transparency = attr(mat, "transparency").and_then(|v| v.parse().ok()).unwrap_or(0.0);
            }
        } else {
            m.base_color = vec3(attr(el, "baseColor"), Vec3::ONE);
            m.roughness = attr(el, "roughness").and_then(|v| v.parse().ok()).unwrap_or(0.0);
            m.metalness = attr(el, "metalness").and_then(|v| v.parse().ok()).unwrap_or(1.0);
            let intensity: f32 = attr(el, "emissiveIntensity").and_then(|v| v.parse().ok()).unwrap_or(1.0);
            m.emissive = vec3(attr(el, "emissiveColor"), Vec3::ZERO) * intensity;
            m.transparency = attr(el, "transparency").and_then(|v| v.parse().ok()).unwrap_or(0.0);
        }
        for child in el.descendants().filter(|c| c.is_element()) {
            match child.tag_name().name() {
                "ImageTexture"
                    if attr(child, "role").is_none_or(|r| r == "baseColorMap") =>
                {
                    m.base_color_map = attr(child, "url").map(|u| u.trim_matches('"').to_string());
                }
                "TextureTransform" => {
                    let s = attr(child, "scale").map(floats).unwrap_or_default();
                    if s.len() >= 2 {
                        m.texture_scale = Vec2::new(s[0], s[1]);
                    }
                }
                _ => {}
            }
        }
        self.scene.materials.push(m);
        let index = self.scene.materials.len() - 1;
        if let Some(id) = attr(el, "id") {
            self.materials_by_id.insert(id.to_string(), index);
        }
        index
    }

    fn geometry(&mut self, el: roxmltree::Node<'a, 'input>) -> Option<usize> {
        if let Some(used) = use_ref(el) {
            if let Some(&mesh) = self.meshes_by_id.get(&used) {
                return Some(mesh);
            }
            return self.defs.get(used.as_str()).copied().and_then(|def| self.geometry(def));
        }
        let f = |name: &str, default: f32| attr(el, name).and_then(|v| v.parse().ok()).unwrap_or(default);
        let mesh = match el.tag_name().name() {
            "Box" => mesh::box_mesh(vec3(attr(el, "size"), Vec3::splat(0.1))),
            "Plane" => {
                let s = attr(el, "size").map(floats).unwrap_or_default();
                mesh::plane_mesh(if s.len() >= 2 { Vec2::new(s[0], s[1]) } else { Vec2::ONE })
            }
            "Sphere" => mesh::sphere_mesh(f("radius", 0.1), f("subdivision", 1.0) as u32),
            "Cylinder" => mesh::cylinder_mesh(
                f("height", 0.1),
                f("radius", 0.1),
                flag(el, "side"),
                flag(el, "top"),
                flag(el, "bottom"),
            ),
            "Capsule" => mesh::capsule_mesh(
                f("height", 0.1),
                f("radius", 0.1),
                flag(el, "side"),
                flag(el, "top"),
                flag(el, "bottom"),
            ),
            "IndexedFaceSet" => self.indexed_face_set(el),
            _ => return None,
        };
        if mesh.is_empty() {
            return None;
        }
        self.scene.meshes.push(mesh);
        let index = self.scene.meshes.len() - 1;
        if let Some(id) = attr(el, "id") {
            self.meshes_by_id.insert(id.to_string(), index);
        }
        Some(index)
    }

    fn indexed_face_set(&mut self, el: roxmltree::Node<'a, 'input>) -> Mesh {
        let ints = |name: &str| -> Vec<i64> {
            attr(el, name)
                .map(|s| s.split_whitespace().filter_map(|t| t.parse().ok()).collect())
                .unwrap_or_default()
        };
        let mut points = Vec::new();
        let mut tex_points = Vec::new();
        for child in el.children().filter(|c| c.is_element()) {
            match child.tag_name().name() {
                "Coordinate" => {
                    let child = match use_ref(child) {
                        Some(used) if !self.coords_by_id.contains_key(&used) => {
                            self.defs.get(used.as_str()).copied().unwrap_or(child)
                        }
                        _ => child,
                    };
                    points = if let Some(used) = use_ref(child) {
                        self.coords_by_id.get(&used).cloned().unwrap_or_default()
                    } else {
                        let p: Vec<Vec3> = attr(child, "point")
                            .map(floats)
                            .unwrap_or_default()
                            .chunks_exact(3)
                            .map(|c| Vec3::new(c[0], c[1], c[2]))
                            .collect();
                        if let Some(id) = attr(child, "id") {
                            self.coords_by_id.insert(id.to_string(), p.clone());
                        }
                        p
                    }
                }
                "TextureCoordinate" => {
                    tex_points = attr(child, "point")
                        .map(floats)
                        .unwrap_or_default()
                        .chunks_exact(2)
                        .map(|c| Vec2::new(c[0], c[1]))
                        .collect();
                }
                _ => {}
            }
        }
        mesh::indexed_face_set(
            &points,
            &ints("coordIndex"),
            &tex_points,
            &ints("texCoordIndex"),
            attr(el, "creaseAngle").and_then(|v| v.parse().ok()).unwrap_or(0.0),
            flag(el, "ccw"),
        )
    }
}
