//! Axum-style PVA RPC service framework.
//!
//! The PVA wire format treats RPC as a structured request struct
//! travelling on `Command::Rpc`. Without a framework every service
//! author has to:
//!
//! 1. Decode the request struct field-by-field
//! 2. Run the business logic
//! 3. Build a response struct field-by-field
//!
//! The [`PvaService`] trait + [`#[pva_service]`](epics_macros_rs)
//! attribute macro hide steps 1 and 3. Service authors write plain
//! `async fn`s with typed arguments and typed return values; the
//! generated dispatch code does the wire ↔ Rust translation.
//!
//! # Example
//!
//! ```ignore
//! use epics_pva_rs::service::pva_service;
//! use epics_pva_rs::service::types::Status;
//!
//! struct MotorService { driver: Driver }
//!
//! #[pva_service]
//! impl MotorService {
//!     /// `motor:move(target, velocity)` — returns the achieved position
//!     async fn r#move(&self, target: f64, velocity: f64) -> Result<f64, String> {
//!         self.driver.start(target, velocity).await.map_err(|e| e.to_string())
//!     }
//!
//!     /// `motor:stop()` — returns OK / ERROR string
//!     async fn stop(&self) -> Result<Status, String> {
//!         self.driver.halt().await.map_err(|e| e.to_string())?;
//!         Ok(Status::ok())
//!     }
//! }
//!
//! // Register: every method is exposed as `<prefix>:<method_name>`
//! let server = PvaServer::start_with_service("motor", MotorService::new(driver), config);
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::pvdata::{FieldDesc, PvField};

pub mod types;

pub use types::{ServiceArg, ServiceError, ServiceResponse, Status};

/// Boxed future returning a typed RPC response. Generated dispatch
/// code returns this from each method.
pub type ServiceFuture =
    Pin<Box<dyn Future<Output = Result<ServiceResponse, ServiceError>> + Send>>;

/// Trait every `#[pva_service]`-annotated impl satisfies. Most users
/// don't implement it manually — the attribute macro generates the
/// `methods()` table and the dispatch wiring. [`add_rpc_service`]
/// iterates the table and registers each method as a SharedPV under
/// `<prefix>:<method_name>`.
pub trait PvaService: Send + Sync + 'static {
    /// List of methods this service exposes. Each entry pairs a
    /// method name (`"move"`, `"stop"`) with a dispatch fn the
    /// server calls when an RPC arrives at the corresponding PV.
    fn methods(self: Arc<Self>) -> Vec<ServiceMethod>;
}

/// One method exposed by a [`PvaService`]. The framework converts
/// each entry into a SharedPV with an `on_rpc` handler that decodes
/// the request struct, calls `dispatch`, and encodes the response.
pub struct ServiceMethod {
    pub name: String,
    /// Dispatch function: receives the decoded request struct,
    /// returns a typed response future.
    pub dispatch: Arc<dyn Fn(PvField) -> ServiceFuture + Send + Sync>,
}

/// Argument set ergonomics for hand-written services. Provides a
/// uniform way to pull positional arguments out of an incoming
/// PV-request struct without writing boilerplate match arms. The
/// `#[pva_service]` macro emits equivalent code under the hood.
///
/// ```ignore
/// let args = Args::from_pv_field(&request);
/// let target: f64 = args.get_named("target")?;
/// let velocity: f64 = args.get_named_or("velocity", 1.0);
/// ```
pub struct Args {
    by_name: HashMap<String, PvField>,
}

impl Args {
    pub fn from_pv_field(field: &PvField) -> Self {
        let mut by_name = HashMap::new();
        if let PvField::Structure(s) = field {
            // Special-case NTURI: arguments live in `query.<name>`.
            if let Some(PvField::Structure(query)) = s.get_field("query") {
                for (k, v) in &query.fields {
                    by_name.insert(k.clone(), v.clone());
                }
            } else {
                for (k, v) in &s.fields {
                    by_name.insert(k.clone(), v.clone());
                }
            }
        }
        Self { by_name }
    }

    /// Pull a named argument with full type checking. The
    /// `#[pva_service]` macro emits this for every typed method
    /// parameter.
    pub fn get_named<T: ServiceArg>(&self, name: &str) -> Result<T, ServiceError> {
        let raw = self
            .by_name
            .get(name)
            .ok_or_else(|| ServiceError::MissingArg(name.into()))?;
        T::from_pv_field(raw).map_err(|e| ServiceError::WrongArgType(name.into(), e.to_string()))
    }

    pub fn get_named_or<T: ServiceArg>(&self, name: &str, default: T) -> T {
        self.by_name
            .get(name)
            .and_then(|f| T::from_pv_field(f).ok())
            .unwrap_or(default)
    }
}

/// Register every method of `service` under `<prefix>:<method>`
/// against the given [`crate::server_native::SharedSource`]. Each
/// method becomes a SharedPV whose `on_rpc` handler routes the
/// incoming request through the generated dispatch.
///
/// Returns the names that were registered for diagnostics.
pub fn add_rpc_service<S: PvaService>(
    source: &crate::server_native::SharedSource,
    prefix: &str,
    service: S,
) -> Vec<String> {
    let arc = Arc::new(service);
    let mut registered = Vec::new();
    for method in PvaService::methods(arc.clone()) {
        let pv_name = if prefix.is_empty() {
            method.name.clone()
        } else {
            format!("{prefix}:{}", method.name)
        };
        let pv = crate::server_native::SharedPV::new();
        // Open with a generic Variant descriptor so any struct
        // can flow in/out of this RPC slot. Concrete responses
        // carry their own descriptor (encoded by the framework).
        // The slot has no concrete current value yet — seed it with a
        // null `any` so the value is consistent with the Variant root
        // (open() now enforces value_matches_descriptor; a placeholder
        // NTRPC struct under a Variant descriptor is exactly the
        // descriptor/value mismatch that guard rejects).
        // Freshly constructed PV: this is the only open() and cannot
        // collide with a prior one, so the close()-first guard never trips.
        pv.open(FieldDesc::Variant, PvField::Null)
            .expect("freshly built service PV opens");
        let dispatch = method.dispatch.clone();
        // Use the async on_rpc variant so dispatch runs on the
        // calling task's runtime — no `block_in_place` (which
        // panics on single-threaded runtimes) and no
        // `block_on` (which can deadlock on current-thread
        // executors).
        pv.on_rpc_async(move |_pv, _req_desc, req_value| {
            let dispatch = dispatch.clone();
            async move {
                match dispatch(req_value).await {
                    Ok(resp) => Ok((resp.descriptor, resp.value)),
                    Err(e) => Err(e.to_string()),
                }
            }
        });
        source.add(&pv_name, pv);
        registered.push(pv_name);
    }
    registered
}

/// Tear down a previously-registered service. Pass the same
/// `prefix` and the list of method names returned from
/// [`add_rpc_service`]; each entry is removed from `source` so the
/// PVs disappear from search responses and any further RPC frames
/// surface as "unknown PV". Idempotent — names not present in the
/// source are silently ignored.
pub fn remove_rpc_service(
    source: &crate::server_native::SharedSource,
    registered_names: &[String],
) {
    for name in registered_names {
        let _ = source.remove(name);
    }
}

/// Re-export the attribute macro under the conventional path
/// `epics_pva_rs::service::pva_service`.
pub use epics_macros_rs::pva_service;
