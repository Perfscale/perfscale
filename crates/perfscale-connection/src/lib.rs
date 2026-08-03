//! Named-connection registry for step-based load engines.
//!
//! # The pattern
//!
//! Step-based test engines share one lifecycle across every stateful
//! protocol family (WebSocket, gRPC, SQL databases, …):
//!
//! 1. **Connect** — a `*-connect` step opens a live handle. The handle is
//!    not JSON, so it cannot live in the step-variable store; instead it is
//!    *parked* in a registry and the step returns a JSON-safe
//!    **Connection ID** (`"ws-1"`, `"grpc-2"`, `"db-1"`, …).
//! 2. **Use** — later steps reference the handle by that id
//!    (`conn: ${{ connect_step.id }}`). Each step *takes* the handle out for
//!    exclusive use and *puts it back* when done, so no lock is ever held
//!    across an `.await` and a double-reference fails with a clean
//!    "unknown id" error instead of a deadlock.
//! 3. **Close** — an explicit `*-close` step takes the handle and does not
//!    put it back; whatever a scenario leaves open is dropped when the
//!    registry is drained at the end of the iteration.
//!
//! This crate is that registry, factored out of the engine so every family
//! (and downstream `pro/*` actions) shares one implementation — and one set
//! of semantics — instead of re-rolling it per protocol.
//!
//! # Design decisions
//!
//! **Generics, not boxed trait objects.** A [`ConnectionRegistry<C>`] is
//! generic over the parked handle type `C: `[`Connection`]. The alternative —
//! storing `Box<dyn Connection>` — would force every `take`/`put_back` to
//! downcast back to the concrete type, and a failed downcast (asking a
//! WebSocket registry for a DB handle) would have to either panic or
//! silently lose the handle. Generics turn that whole failure mode into a
//! compile error, cost nothing at runtime, and keep the registry itself free
//! of any protocol dependency.
//!
//! **One registry per family, keyed by id prefix.** Each family gets its own
//! registry built with its id prefix (`"ws"`, `"grpc"`, `"db"`, …). Ids are
//! minted as `{prefix}-{n}` with a per-registry counter, so families never
//! collide and every family keeps exactly the id format its users already
//! see in outputs and logs. Draining one registry is therefore inherently a
//! drain-by-prefix: the engine drains each family's registry at iteration
//! end while non-connection caches (e.g. a schema cache keyed by URL) live
//! outside the registry and survive.
//!
//! **No async, no close logic, no dependencies.** Steps run strictly
//! sequentially within a VU, so the registry never holds a lock across an
//! `.await` — it does not need to know an async runtime exists. Closing is
//! likewise left to the families: [`Connection::close`] is a graceful-close
//! hook the *caller* invokes (e.g. a WebSocket Close handshake) before
//! dropping the id, while [`ConnectionRegistry::drain`] simply drops
//! whatever is left — an abrupt teardown by design, because a parked handle
//! must never outlive its iteration. That is why the crate depends on
//! nothing but `std`.
//!
//! # Example
//!
//! ```
//! use perfscale_connection::{Connection, ConnectionRegistry};
//!
//! struct MySocket { url: String /* + real socket state */ }
//!
//! impl Connection for MySocket {
//!     fn label(&self) -> &str { &self.url }
//!     // `close` defaults to plain drop; override for a graceful shutdown.
//! }
//!
//! let registry = ConnectionRegistry::<MySocket>::new("ws");
//! let id = registry.insert(MySocket { url: "wss://example.com".into() });
//! assert_eq!(id, "ws-1");
//!
//! // A later step takes exclusive ownership…
//! let conn = registry.take(&id).expect("parked");
//! assert!(registry.take(&id).is_none(), "gone while in use");
//! // …and puts it back when done.
//! registry.put_back(&id, conn);
//!
//! assert_eq!(registry.drain(), 1, "iteration end drops what is left");
//! ```

mod connection;
mod registry;

pub use connection::Connection;
pub use registry::ConnectionRegistry;
