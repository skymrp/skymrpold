#[derive(Debug)]
pub struct Mrp {
    /// Name (for debugging purposes and sorting)
    pub name: String,
    /// Address/program counter value for the entry point.
    pub entry_point_pc: Option<u32>,
}
