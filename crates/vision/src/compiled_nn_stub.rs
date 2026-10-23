//! Stand-in for `compiled_nn` on non-x86_64 hosts (e.g. Apple Silicon simulation).
//!
//! CompiledNN JIT-compiles networks to x86-64 machine code, so it cannot run here. This stub keeps
//! the same API and always outputs zeros, which ball detection reads as "no ball".

use std::path::Path;

pub struct Tensor {
    pub data: Vec<f32>,
}

pub struct CompiledNN {
    input: Tensor,
    output: Tensor,
}

impl Default for CompiledNN {
    fn default() -> Self {
        Self {
            input: Tensor {
                data: vec![0.0; 64 * 64 * 3],
            },
            output: Tensor { data: vec![0.0; 8] },
        }
    }
}

impl CompiledNN {
    pub fn compile(&mut self, _path: impl AsRef<Path>) {}

    pub fn input_mut(&mut self, _index: usize) -> &mut Tensor {
        &mut self.input
    }

    pub fn apply(&mut self) {}

    pub fn output(&self, _index: usize) -> &Tensor {
        &self.output
    }
}
