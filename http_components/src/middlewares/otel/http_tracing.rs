use super::{
    attributes::trace_attributes_from_request, extractor::HTTPExtractor, keys::HTTP_STATUS_CODE,
};
use actix_web::{
    dev::{Service, ServiceRequest, ServiceResponse, Transform},
    Error,
};
use futures_util::future::{ok, FutureExt as _, LocalBoxFuture, Ready};
use opentelemetry::{
    global::{self, BoxedTracer},
    trace::{FutureExt, SpanKind, Status, TraceContextExt, Tracer},
};
use std::{borrow::Cow, task::Poll};

#[derive(Default, Debug)]
pub struct OtelTracing {}

impl OtelTracing {
    pub fn new() -> OtelTracing {
        OtelTracing::default()
    }
}

impl<S, B> Transform<S, ServiceRequest> for OtelTracing
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = OtelTracingMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(OtelTracingMiddleware::new(service, None))
    }
}

pub struct OtelTracingMiddleware<S> {
    service: S,
    tracer: BoxedTracer,
}

impl<S, B> OtelTracingMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    fn new(service: S, _: Option<B>) -> Self {
        OtelTracingMiddleware {
            service,
            tracer: global::tracer("actix-web-middleware"),
        }
    }
}

impl<S, B> Service<ServiceRequest> for OtelTracingMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&self, mut req: ServiceRequest) -> Self::Future {
        let parent_context = global::get_text_map_propagator(|propagator| {
            propagator.extract(&HTTPExtractor::new(req.headers_mut()))
        });
        let http_route: Cow<'static, str> = req
            .match_pattern()
            .map(Into::into)
            .unwrap_or_else(|| "default".into());

        let mut builder = self.tracer.span_builder(http_route.clone());
        builder.span_kind = Some(SpanKind::Server);
        builder.attributes = Some(trace_attributes_from_request(&req, &http_route));

        let span = self.tracer.build_with_context(builder, &parent_context);
        let cx = parent_context.with_span(span);

        let fut = self
            .service
            .call(req)
            .with_context(cx.clone())
            .map(move |res| match res {
                Ok(ok_res) => {
                    let span = cx.span();
                    span.set_attribute(HTTP_STATUS_CODE.i64(ok_res.status().as_u16() as i64));
                    if ok_res.status().is_server_error() {
                        span.set_status(Status::error(
                            ok_res
                                .status()
                                .canonical_reason()
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                        ));
                    };
                    span.end();
                    Ok(ok_res)
                }
                Err(err) => {
                    let span = cx.span();
                    span.set_status(Status::error(format!("{:?}", err)));
                    span.end();
                    Err(err)
                }
            });

        Box::pin(async move { fut.await })
    }
}
