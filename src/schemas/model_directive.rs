/// A model request in the user text, such as "use opus" or "switch to auto".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelDirective {
    /// The user names a model. The names are tried in order, the most specific first.
    Model(Vec<String>),
    /// The user hands the choice back to Smart Auto.
    Auto,
}
