/// USD cost of one estimate, split by token component.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CostParts {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    /// The whole cost when the long-context tier applied, else zero.
    pub long_tier: f64,
}

impl CostParts {
    /// Sum of the four components. `long_tier` is a tag, not an addend.
    #[must_use]
    pub fn total(self) -> f64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

impl std::ops::AddAssign for CostParts {
    fn add_assign(&mut self, rhs: Self) {
        self.input += rhs.input;
        self.output += rhs.output;
        self.cache_read += rhs.cache_read;
        self.cache_write += rhs.cache_write;
        self.long_tier += rhs.long_tier;
    }
}
