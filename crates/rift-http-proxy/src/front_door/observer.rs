//! Observes route dispatches on the front door (issue #368): the seam a clustered embedder hangs
//! a per-route hit counter on, so an operator's route table can show a HITS column — the column
//! that answers "which of these routes is doing anything", where a zero means the route is wrong
//! or dead.
//!
//! This trait lives beside [`crate::front_door::route_table`], not in `rift-mock-core`, because a
//! *route* is an http-proxy concept. `rift-mock-core` knows imposters and ports, never routes, and
//! putting a route-shaped trait there would be the first thread pulling that boundary apart.
//!
//! The shape mirrors [`rift_mock_core::imposter::journal::RequestJournal`]: an upstream trait an
//! embedder implements, whose method the engine calls on every request. That trait's
//! `note_request(port)` backs a per-imposter request count the same way this one backs a
//! per-route dispatch count — same seam, one level up the addressing.

/// Notified once per request a route claims, whatever happens to that request past that point.
pub trait RouteObserver: Send + Sync {
    /// Called once per request that a route claimed, with that route's `id`.
    ///
    /// Takes the id rather than the whole [`Route`](crate::front_door::route_table::Route)
    /// because the observer's job is to count, not to re-decide anything — handing it the whole
    /// route (its match rules, its target) would invite it to act on them. An id is all a
    /// counter keyed by route needs.
    fn note_dispatch(&self, route_id: &str);
}
