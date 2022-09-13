//! Implements the Execution phase of the request lifecycle.

use std::sync::Arc;
use std::task::Poll;

use futures::future::ready;
use futures::future::BoxFuture;
use futures::stream::once;
use futures::StreamExt;
use tower::BoxError;
use tower::ServiceBuilder;
use tower::ServiceExt;
use tower_service::Service;
use tracing::Instrument;

use super::execution::QueryPlan;
use super::layers::allow_only_http_post_mutations::AllowOnlyHttpPostMutationsLayer;
use super::new_service::NewService;
use super::subgraph_service::SubgraphServiceFactory;
use super::Plugins;
use crate::graphql::IncrementalResponse;
use crate::graphql::Response;
use crate::json_ext::Object;
use crate::json_ext::ValueExt;
use crate::services::execution;
use crate::ExecutionRequest;
use crate::ExecutionResponse;
use crate::Schema;

/// [`Service`] for query execution.
#[derive(Clone)]
pub(crate) struct ExecutionService<SF: SubgraphServiceFactory> {
    pub(crate) schema: Arc<Schema>,
    pub(crate) subgraph_creator: Arc<SF>,
}

impl<SF> Service<ExecutionRequest> for ExecutionService<SF>
where
    SF: SubgraphServiceFactory,
{
    type Response = ExecutionResponse;
    type Error = BoxError;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        // We break backpressure here.
        // We can implement backpressure, but we need to think about what we want out of it.
        // For instance, should be block all services if one downstream service is not ready?
        // This may not make sense if you have hundreds of services.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: ExecutionRequest) -> Self::Future {
        let this = self.clone();
        let fut = async move {
            let context = req.context;
            let ctx = context.clone();
            let (sender, receiver) = futures::channel::mpsc::channel(10);
            let variables = req.supergraph_request.body().variables.clone();
            let operation_name = req.supergraph_request.body().operation_name.clone();

            let first = req
                .query_plan
                .execute(
                    &context,
                    &this.subgraph_creator,
                    &Arc::new(req.supergraph_request),
                    &this.schema,
                    sender,
                )
                .await;

            let rest = receiver;

            let query = req.query_plan.query.clone();
            let stream = once(ready(first)).chain(rest).boxed();

            let can_be_deferred = req.query_plan.root.contains_defer();
            let schema = this.schema.clone();

            let stream = stream
                .map(move |mut response: Response| {
                    let has_next = response.has_next.unwrap_or(true);
                    tracing::debug_span!("format_response").in_scope(|| {
                        query.format_response(
                            &mut response,
                            operation_name.as_deref(),
                            variables.clone(),
                            schema.api_schema(),
                        )
                    });

                    match (response.path.as_ref(), response.data.as_ref()) {
                        (None, _) | (_, None) => {
                            if can_be_deferred {
                                response.has_next = Some(has_next);
                            }

                            response
                        }
                        // if the deferred response specified a path, we must extract the
                        // values matched by that path and create a separate response for
                        // each of them.
                        // While { "data": { "a": { "b": 1 } } } and { "data": { "b": 1 }, "path: ["a"] }
                        // would merge in the same ways, some clients will generate code
                        // that checks the specific type of the deferred response at that
                        // path, instead of starting from the root object, so to support
                        // this, we extract the value at that path.
                        // In particular, that means that a deferred fragment in an object
                        // under an array would generate one response par array element
                        (Some(response_path), Some(response_data)) => {
                            let mut sub_responses = Vec::new();
                            response_data.select_values_and_paths(response_path, |path, value| {
                                sub_responses.push((path.clone(), value.clone()));
                            });

                            Response::builder()
                                .has_next(has_next)
                                .incremental(
                                    sub_responses
                                        .into_iter()
                                        .map(move |(path, data)| {
                                            IncrementalResponse::builder()
                                                .and_label(response.label.clone())
                                                .data(data)
                                                .path(path)
                                                .errors(response.errors.clone())
                                                .extensions(response.extensions.clone())
                                                .build()
                                        })
                                        .collect(),
                                )
                                .build()
                        }
                    }
                })
                .boxed();

            Ok(ExecutionResponse::new_from_response(
                http::Response::new(stream as _),
                ctx,
            ))
        }
        .in_current_span();
        Box::pin(fut)
    }
}

/*fn process_execution_response(
    execution_response: ExecutionResponse,
    plan: Arc<QueryPlan>,
    operation_name: Option<String>,
    variables: Object,
    schema: Arc<Schema>,
    can_be_deferred: bool,
) -> Result<ExecutionResponse, BoxError> {
    let ExecutionResponse { response, context } = execution_response;

    let (parts, response_stream) = response.into_parts();
    let stream = response_stream.map(move |mut response: Response| {
        let has_next = response.has_next.unwrap_or(true);
        tracing::debug_span!("format_response").in_scope(|| {
            plan.query.format_response(
                &mut response,
                operation_name.as_deref(),
                variables.clone(),
                schema.api_schema(),
            )
        });

        match (response.path.as_ref(), response.data.as_ref()) {
            (None, _) | (_, None) => {
                if can_be_deferred {
                    response.has_next = Some(has_next);
                }

                response
            }
            // if the deferred response specified a path, we must extract the
            //values matched by that path and create a separate response for
            //each of them.
            // While { "data": { "a": { "b": 1 } } } and { "data": { "b": 1 }, "path: ["a"] }
            // would merge in the same ways, some clients will generate code
            // that checks the specific type of the deferred response at that
            // path, instead of starting from the root object, so to support
            // this, we extract the value at that path.
            // In particular, that means that a deferred fragment in an object
            // under an array would generate one response par array element
            (Some(response_path), Some(response_data)) => {
                let mut sub_responses = Vec::new();
                response_data.select_values_and_paths(response_path, |path, value| {
                    sub_responses.push((path.clone(), value.clone()));
                });

                Response::builder()
                    .has_next(has_next)
                    .incremental(
                        sub_responses
                            .into_iter()
                            .map(move |(path, data)| {
                                IncrementalResponse::builder()
                                    .and_label(response.label.clone())
                                    .data(data)
                                    .path(path)
                                    .errors(response.errors.clone())
                                    .extensions(response.extensions.clone())
                                    .build()
                            })
                            .collect(),
                    )
                    .build()
            }
        }
    });

    Ok(SupergraphResponse {
        context,
        response: http::Response::from_parts(
            parts,
            if can_be_deferred {
                stream.left_stream()
            } else {
                stream.right_stream()
            }
            .in_current_span()
            .boxed(),
        ),
    })
}*/

pub(crate) trait ExecutionServiceFactory:
    NewService<ExecutionRequest, Service = Self::ExecutionService> + Clone + Send + 'static
{
    type ExecutionService: Service<
            ExecutionRequest,
            Response = ExecutionResponse,
            Error = BoxError,
            Future = Self::Future,
        > + Send;
    type Future: Send;
}

#[derive(Clone)]
pub(crate) struct ExecutionCreator<SF: SubgraphServiceFactory> {
    pub(crate) schema: Arc<Schema>,
    pub(crate) plugins: Arc<Plugins>,
    pub(crate) subgraph_creator: Arc<SF>,
}

impl<SF> NewService<ExecutionRequest> for ExecutionCreator<SF>
where
    SF: SubgraphServiceFactory,
{
    type Service = execution::BoxService;

    fn new_service(&self) -> Self::Service {
        ServiceBuilder::new()
            .layer(AllowOnlyHttpPostMutationsLayer::default())
            .service(
                self.plugins.iter().rev().fold(
                    crate::services::execution_service::ExecutionService {
                        schema: self.schema.clone(),
                        subgraph_creator: self.subgraph_creator.clone(),
                    }
                    .boxed(),
                    |acc, (_, e)| e.execution_service(acc),
                ),
            )
            .boxed()
    }
}

impl<SF: SubgraphServiceFactory> ExecutionServiceFactory for ExecutionCreator<SF> {
    type ExecutionService = execution::BoxService;
    type Future = <<ExecutionCreator<SF> as NewService<ExecutionRequest>>::Service as Service<
        ExecutionRequest,
    >>::Future;
}
