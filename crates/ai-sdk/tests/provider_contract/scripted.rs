use futures::future::BoxFuture;
use stakit_ai_sdk::{
    AssistantContent, CacheCapabilities, CacheTtl, ChatRequest, ChatResponse, EventStream,
    ExplicitCacheCapabilities, Message, Provider, ProviderCapabilities, ProviderError, StopReason,
    StreamEvent, Usage, event_stream, test_support::contract::ErrorStage,
};

pub(super) struct ScriptedProvider {
    pub(super) events: Vec<StreamEvent>,
    pub(super) failure: Option<ErrorStage>,
}
pub(super) const fn usage() -> Usage {
    Usage {
        input_tokens: 10,
        output_tokens: 4,
        reasoning_tokens: 1,
        ..Usage::zero()
    }
}
pub(super) const fn end() -> StreamEvent {
    StreamEvent::End {
        stop: StopReason::EndTurn,
        usage: usage(),
    }
}
pub(super) fn normal() -> ScriptedProvider {
    ScriptedProvider {
        events: vec![
            StreamEvent::Start {
                usage: Usage {
                    input_tokens: 10,
                    ..Usage::zero()
                },
            },
            StreamEvent::TextDelta("hello".into()),
            end(),
        ],
        failure: None,
    }
}
pub(super) fn request() -> ChatRequest {
    let mut request = ChatRequest::new("custom");
    request.messages = vec![Message::user("hello")];
    request
}
fn failure() -> ProviderError {
    ProviderError::Decode {
        err: "private diagnostic".into(),
        body: "private payload".into(),
    }
}
impl Provider for ScriptedProvider {
    fn model_id(&self) -> &'static str {
        "custom"
    }
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            sampling_params: false,
            max_output_tokens: Some(16),
            ..ProviderCapabilities::default()
        }
    }
    fn cache_capabilities(&self) -> CacheCapabilities {
        CacheCapabilities::Known {
            implicit: false,
            explicit: Some(ExplicitCacheCapabilities {
                max_points: 2,
                ttls: vec![CacheTtl::FiveMin],
            }),
        }
    }
    fn complete(
        &self,
        _request: ChatRequest,
    ) -> BoxFuture<'_, Result<ChatResponse, ProviderError>> {
        Box::pin(async {
            if self.failure.is_some() {
                return Err(failure());
            }
            Ok(ChatResponse {
                content: vec![AssistantContent::Text("hello".into())],
                stop: StopReason::EndTurn,
                usage: usage(),
            })
        })
    }
    fn stream(&self, _request: ChatRequest) -> BoxFuture<'_, Result<EventStream, ProviderError>> {
        Box::pin(async {
            match self.failure {
                Some(ErrorStage::Acquisition) => Err(failure()),
                Some(ErrorStage::Streaming) => Ok(event_stream(vec![
                    Ok(StreamEvent::Start {
                        usage: Usage::zero(),
                    }),
                    Err(failure()),
                ])),
                None => Ok(event_stream(
                    self.events.clone().into_iter().map(Ok).collect(),
                )),
            }
        })
    }
}
