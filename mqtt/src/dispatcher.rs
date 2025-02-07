use async_trait::async_trait;
use futures_util::StreamExt;
use messaging::{
    dispatcher::{Dispatcher, DispatcherDefinition},
    errors::MessagingError,
    handler::{ConsumerHandler, ConsumerPayload},
};
use opentelemetry::{
    global::{self, BoxedTracer},
    trace::{SpanKind, Status, TraceContextExt},
    Context,
};
use paho_mqtt::{AsyncClient, AsyncReceiver, Message};
use std::{borrow::Cow, sync::Arc};
use tracing::{debug, error, warn};

pub struct MQTTDispatcher {
    conn: Arc<AsyncClient>,
    stream: AsyncReceiver<Option<Message>>,
    tracer: BoxedTracer,
    topics: Vec<String>,
    handlers: Vec<Arc<dyn ConsumerHandler>>,
}

impl MQTTDispatcher {
    pub fn new(conn: Arc<AsyncClient>, stream: AsyncReceiver<Option<Message>>) -> Self {
        Self {
            conn,
            stream,
            tracer: global::tracer("mqtt-consumer"),
            topics: vec![],
            handlers: vec![],
        }
    }
}

#[async_trait]
impl Dispatcher for MQTTDispatcher {
    fn register(
        mut self,
        definition: &DispatcherDefinition,
        handler: Arc<dyn ConsumerHandler>,
    ) -> Self {
        if definition.name.is_empty() {
            warn!("cant create dispatcher with empty topic");
            return self;
        }

        self.topics.push(definition.name.clone());
        self.handlers.push(handler);

        self
    }

    async fn consume_blocking(&self) -> Result<(), MessagingError> {
        for topic in self.topics.clone() {
            self.conn.subscribe(topic, 2);
        }

        let mut cloned_stream = self.stream.clone();

        while let Some(delivery) = cloned_stream.next().await {
            match delivery {
                Some(msg) => match self.consume(&Context::new(), &msg).await {
                    Err(e) => error!(error = e.to_string(), "failure to consume msg"),
                    _ => {}
                },
                _ => {}
            }
        }

        Ok(())
    }
}

impl MQTTDispatcher {
    async fn consume(&self, ctx: &Context, msg: &Message) -> Result<(), MessagingError> {
        let handler_idx = self.get_handler_index(ctx, msg.topic())?;

        let ctx = traces::span_ctx(&self.tracer, SpanKind::Consumer, msg.topic());
        let span = ctx.span();

        debug!(
            trace.id = traces::trace_id(&ctx),
            span.id = traces::span_id(&ctx),
            "message received in a topic {:?}",
            msg.topic()
        );

        let handler = self.handlers.get(handler_idx).unwrap();

        let payload = ConsumerPayload {
            from: msg.topic().to_owned(),
            msg_type: String::new(),
            payload: msg.payload().into(),
            headers: None,
        };

        return match handler.exec(&ctx, &payload).await {
            Ok(_) => {
                debug!(
                    trace.id = traces::trace_id(&ctx),
                    span.id = traces::span_id(&ctx),
                    "event processed successfully"
                );
                Ok(())
            }
            Err(e) => {
                debug!(
                    trace.id = traces::trace_id(&ctx),
                    span.id = traces::span_id(&ctx),
                    "failed to handle the event - {:?}",
                    e
                );
                span.record_error(&e);
                span.set_status(Status::Error {
                    description: Cow::from("failed to handle the event"),
                });
                Err(e)
            }
        };
    }

    fn get_handler_index(
        &self,
        ctx: &Context,
        received_topic: &str,
    ) -> Result<usize, MessagingError> {
        let mut p = usize::MAX;

        'outer: for handler_topic_index in 0..self.topics.len() {
            let handler_topic = self.topics[handler_topic_index].clone();

            if received_topic == handler_topic {
                p = handler_topic_index;
                break;
            }

            if received_topic.len() > received_topic.len() {
                break;
            }

            let saved_topic_fields: Vec<_> = handler_topic.split('/').collect();
            let received_topic_fields: Vec<_> = received_topic.split('/').collect();

            for i in 0..saved_topic_fields.len() {
                if saved_topic_fields[i] == "#" {
                    p = handler_topic_index;
                    break 'outer;
                }

                if saved_topic_fields[i] != "+" && saved_topic_fields[i] != received_topic_fields[i]
                {
                    break 'outer;
                }

                if saved_topic_fields[i] == "+" && i == saved_topic_fields.len() - 1 {
                    p = handler_topic_index;
                    break 'outer;
                }
            }

            if saved_topic_fields.len() == received_topic_fields.len() {
                p = handler_topic_index;
                break;
            }
        }

        if p == usize::MAX {
            warn!(
                trace.id = traces::trace_id(&ctx),
                span.id = traces::span_id(&ctx),
                topic = received_topic,
                "cant find dispatch for this topic"
            );
            return Err(MessagingError::UnregisteredHandler);
        }

        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use messaging::handler::MockConsumerHandler;
    use paho_mqtt::CreateOptions;
    use std::vec;

    #[test]
    fn test_new() {
        let mut client = AsyncClient::new(CreateOptions::default()).unwrap();
        let stream = client.get_stream(2048);
        MQTTDispatcher::new(Arc::new(client), stream);
    }

    #[test]
    fn test_declare() {
        let mut client = AsyncClient::new(CreateOptions::default()).unwrap();
        let stream = client.get_stream(2048);
        let dispatch = MQTTDispatcher::new(Arc::new(client), stream)
            .register(
                &DispatcherDefinition {
                    name: "some/topic".to_owned(),
                    msg_type: String::new(),
                },
                Arc::new(MockConsumerHandler::new()),
            )
            .register(
                &DispatcherDefinition {
                    name: String::new(),
                    msg_type: String::new(),
                },
                Arc::new(MockConsumerHandler::new()),
            );

        assert!(dispatch.handlers.len() == 1);
    }

    #[tokio::test]
    async fn test_consume() {
        let mut client = AsyncClient::new(CreateOptions::default()).unwrap();
        let stream = client.get_stream(2048);

        let mut handler = MockConsumerHandler::new();
        handler.expect_exec().return_once(move |_, _| Ok(()));

        let dispatch = MQTTDispatcher::new(Arc::new(client), stream).register(
            &DispatcherDefinition {
                name: "some/topic/#".to_owned(),
                msg_type: String::new(),
            },
            Arc::new(handler),
        );

        let msg = Message::new("some/topic/sub/1", vec![], 0);

        let res = dispatch.consume(&Context::new(), &msg).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_consume_with_plus_wildcard() {
        let mut client = AsyncClient::new(CreateOptions::default()).unwrap();
        let stream = client.get_stream(2048);

        let mut handler = MockConsumerHandler::new();
        handler.expect_exec().return_once(move |_, _| Ok(()));

        let dispatcher = MQTTDispatcher::new(Arc::new(client), stream).register(
            &DispatcherDefinition {
                name: "some/+/+/sub".to_owned(),
                msg_type: String::new(),
            },
            Arc::new(handler),
        );

        let msg = Message::new("some/topic/with/sub", vec![], 0);

        let res = dispatcher.consume(&Context::new(), &msg).await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn test_consume_with_dispatch_return_err() {
        let mut client = AsyncClient::new(CreateOptions::default()).unwrap();
        let stream = client.get_stream(2048);

        let mut handler = MockConsumerHandler::new();
        handler
            .expect_exec()
            .return_once(move |_, _| Err(MessagingError::ConsumerError("err".to_string())));

        let dispatcher = MQTTDispatcher::new(Arc::new(client), stream).register(
            &DispatcherDefinition {
                name: "/some/topic/#".to_owned(),
                msg_type: String::new(),
            },
            Arc::new(handler),
        );

        let msg = Message::new("/some/topic/sub", vec![], 0);

        let res = dispatcher.consume(&Context::new(), &msg).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn test_consume_with_unregistered_consumer() {
        let mut client = AsyncClient::new(CreateOptions::default()).unwrap();
        let stream = client.get_stream(2048);

        let mut handler = MockConsumerHandler::new();
        handler.expect_exec().return_once(move |_, _| Ok(()));

        let dispatcher = MQTTDispatcher::new(Arc::new(client), stream).register(
            &DispatcherDefinition {
                name: "other/topic/#".to_owned(),
                msg_type: String::new(),
            },
            Arc::new(handler),
        );

        let msg = Message::new("some/topic/sub", vec![], 0);

        let res = dispatcher.consume(&Context::new(), &msg).await;
        assert!(res.is_err());
    }
}
