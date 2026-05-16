use crate::backend::Backend;

pub mod gigabyte_gpu;
pub mod gigabyte_mobo;
pub mod gskill_ddr5;

pub fn all() -> Vec<Box<dyn Backend>> {
    vec![
        Box::new(gigabyte_mobo::GigabyteMobo::new()),
        Box::new(gigabyte_gpu::GigabyteGpu::new()),
        Box::new(gskill_ddr5::GSkillDdr5::new()),
    ]
}
