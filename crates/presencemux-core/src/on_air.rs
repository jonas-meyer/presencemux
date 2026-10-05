/// How much of the person and the room reaches the laptops, from least to most.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OnAir {
    Off,
    /// A prepared still image, such as a slate.
    Canned,
    Live,
}
