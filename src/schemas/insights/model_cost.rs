/// Estimated spend of one model over the insights window.
///
/// `free` is true when the model runs on a local profile, which the hub prices at zero.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelCost {
    pub usd: f64,
    pub free: bool,
}
