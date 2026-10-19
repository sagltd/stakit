use futures::future::BoxFuture;
use stakit_ai_sdk::{
    CacheCapabilities, ChatRequest, ChatResponse, EventStream, Provider, ProviderError, StopReason,
    StreamEvent, Usage, event_stream,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

pub(super) struct CacheProvider {
    pub(super) model: &'static str,
    pub(super) controls: CacheCapabilities,
    pub(super) calls: Arc<AtomicUsize>,
}
impl Provider for CacheProvider {
    fn model_id(&self) -> &str {
        self.model
    }
    fn cache_capabilities(&self) -> CacheCapabilities {
        self.controls.clone()
    }
    fn complete(
        &self,
        _request: ChatRequest,
    ) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async { Err(ProviderError::Cancelled) })
    }
    fn stream(&self, _request: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(event_stream(vec![Ok(StreamEvent::End {
                stop: StopReason::EndTurn,
                usage: Usage::zero(),
            })]))
        })
    }
}
