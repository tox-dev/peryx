use std::sync::Arc;

use axum::http::Method;
use axum::routing::MethodRouter;
use axum::{Extension, Router};

use crate::rate_limit::RouteClass;
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteMethod {
    Delete,
    Get,
    Post,
    Put,
}

impl RouteMethod {
    #[must_use]
    pub fn matches(self, method: &Method) -> bool {
        match self {
            Self::Delete => method == Method::DELETE,
            Self::Get => method == Method::GET || method == Method::HEAD,
            Self::Post => method == Method::POST,
            Self::Put => method == Method::PUT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutePosture {
    Mutation,
    Read,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteRateLimit {
    Class(RouteClass),
    Exempt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteDescriptor {
    method: RouteMethod,
    path: &'static str,
    posture: RoutePosture,
    rate_limit: RouteRateLimit,
}

#[derive(Debug, Clone, Copy)]
pub struct ProcessRouteMethodNotAllowed;

impl RouteDescriptor {
    #[must_use]
    pub const fn new(
        method: RouteMethod,
        path: &'static str,
        posture: RoutePosture,
        rate_limit: RouteRateLimit,
    ) -> Self {
        Self {
            method,
            path,
            posture,
            rate_limit,
        }
    }

    #[must_use]
    pub const fn method(self) -> RouteMethod {
        self.method
    }

    #[must_use]
    pub const fn path(self) -> &'static str {
        self.path
    }

    #[must_use]
    pub const fn posture(self) -> RoutePosture {
        self.posture
    }

    #[must_use]
    pub const fn rate_limit(self) -> RouteRateLimit {
        self.rate_limit
    }
}

pub struct RouteSet {
    router: Router<Arc<AppState>>,
    descriptors: Vec<RouteDescriptor>,
}

impl RouteSet {
    #[must_use]
    pub fn new() -> Self {
        Self {
            router: Router::new(),
            descriptors: Vec::new(),
        }
    }

    #[must_use]
    pub fn route(mut self, descriptor: RouteDescriptor, method_router: MethodRouter<Arc<AppState>>) -> Self {
        self.router = self.router.route(descriptor.path(), method_router);
        self.descriptors.push(descriptor);
        self
    }

    #[must_use]
    pub fn merge(mut self, other: Self) -> Self {
        self.router = self.router.merge(other.router);
        self.descriptors.extend(other.descriptors);
        self
    }

    #[must_use]
    pub fn with_extension<T>(mut self, value: T) -> Self
    where
        T: Clone + Send + Sync + 'static,
    {
        self.router = self.router.layer(Extension(value));
        self
    }

    pub fn into_parts(self) -> (Router<Arc<AppState>>, Vec<RouteDescriptor>) {
        (self.router, self.descriptors)
    }

    pub fn into_router(self) -> Router<Arc<AppState>> {
        self.router
    }
}

impl Default for RouteSet {
    fn default() -> Self {
        Self::new()
    }
}

pub trait HttpRoutes: Send + Sync {
    fn routes(&self) -> RouteSet;
}
