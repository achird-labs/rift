//! The front door: one listener, many imposters, addressed by what the request
//! *says* rather than by a port the client had to know (issue #19 / U-11).
//!
//! The single-port gateway (`/__rift/:port`, issue #212) already lets one port
//! reach every imposter, but the client has to name the target. That is fine for
//! a test harness driving Rift on purpose; it is wrong when the client is an
//! unmodified system under test that believes it is calling
//! `payments.example.com`. The front door closes that gap, and with it the reason
//! to run an nginx sidecar in front of Rift.
//!
//! A request is resolved against the [`route_table`] first. If no route claims
//! it, the gateway's own `/__rift/:port` addressing still works on this listener,
//! so one port serves both styles. Anything left over is a 404 carrying
//! `x-rift-front-door: no-route`, which distinguishes "no route matched" from
//! "a route matched but its imposter is gone" — two failures that look identical
//! from the client side and have completely different fixes.

pub mod listener;
pub mod route_table;

pub use listener::{RunningFrontDoor, bind_front_door};
pub use route_table::{
    CompiledRoutes, HeaderMatch, Route, RouteMatch, RouteTable, RouteTableError, RouteTarget,
};
