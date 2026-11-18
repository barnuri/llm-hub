use std::sync::Arc;

use axum::Router;

use crate::ext::Extension;

/// Builds and runs llm-hub with your own extensions. `llm_hub::run()` is `Hub::new().run()`.
///
/// ```no_run
/// use llm_hub::Hub;
/// use llm_hub::ext::{Extension, Flow, RequestCtx};
///
/// struct TeamGate;
///
/// impl Extension for TeamGate {
///     fn name(&self) -> &str {
///         "team-gate"
///     }
///
///     fn on_request(&self, ctx: &mut RequestCtx<'_>) -> Flow {
///         if ctx.headers().contains_key("x-team") {
///             Flow::Continue
///         } else {
///             Flow::reject(401, "x-team header is required")
///         }
///     }
/// }
///
/// #[tokio::main]
/// async fn main() {
///     Hub::new().extension(TeamGate).run().await;
/// }
/// ```
#[derive(Default)]
pub struct Hub {
    pub(crate) extensions: Vec<Arc<dyn Extension>>,
    pub(crate) routes: Option<Router>,
    pub(crate) router_changes: Vec<Box<dyn FnOnce(Router) -> Router + Send>>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.extensions.iter().map(|ext| ext.name()).collect();
        f.debug_struct("Hub")
            .field("extensions", &names)
            .field("routes", &self.routes.is_some())
            .field("router_changes", &self.router_changes.len())
            .finish()
    }
}

impl Hub {
    /// A hub with only llm-hub's built-in extensions and routes.
    #[must_use]
    pub fn new() -> Hub {
        Hub::default()
    }

    /// Adds an extension. Extensions run after llm-hub's built-ins, in the order they are added,
    /// and before the rules file.
    #[must_use]
    pub fn extension(mut self, extension: impl Extension) -> Hub {
        self.extensions.push(Arc::new(extension));
        self
    }

    /// Serves `routes` under `/ext`. They sit behind the master key like `/api`.
    /// Calling this again merges the routes.
    #[must_use]
    pub fn routes(mut self, routes: Router) -> Hub {
        self.routes = Some(match self.routes.take() {
            Some(existing) => existing.merge(routes),
            None => routes,
        });
        self
    }

    /// Changes the finished router, for example to add a `tower` layer around every route.
    #[must_use]
    pub fn router(mut self, change: impl FnOnce(Router) -> Router + Send + 'static) -> Hub {
        self.router_changes.push(Box::new(change));
        self
    }

    /// Runs a CLI command when one is given, else the server until it is told to stop.
    pub async fn run(self) {
        crate::serve(self).await;
    }
}

#[cfg(test)]
mod tests {
    use axum::routing::get;

    use super::*;

    struct Named;

    impl Extension for Named {
        fn name(&self) -> &'static str {
            "named"
        }
    }

    #[test]
    fn the_builder_collects_extensions_routes_and_router_changes() {
        let hub = Hub::new()
            .extension(Named)
            .extension(Named)
            .routes(Router::new().route("/a", get(|| async { "a" })))
            .routes(Router::new().route("/b", get(|| async { "b" })))
            .router(|router| router);

        assert_eq!(hub.extensions.len(), 2);
        assert!(hub.routes.is_some());
        assert_eq!(hub.router_changes.len(), 1);
    }
}
