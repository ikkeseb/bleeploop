//! A slot's hot swap ([`crate::SlotPort::replace`]): its state. The swap itself is not built yet: a
//! replace goes back `Busy` and a withdraw is answered `NotHeld` (`slots.rs`, `Rack::service`).

/// Where a slot's swap stands. The slot's `side` seat holds a unit exactly when this is not `Idle`.
pub(super) enum Swap {
    /// No swap: the slot plays its `seat` alone.
    Idle,
}
