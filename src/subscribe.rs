use std::borrow::Cow;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::channel::mpsc::{SendError, UnboundedReceiver, UnboundedSender};
use futures::{Sink, Stream};

use chromiumoxide_cdp::cdp::{Event, EventKind, IntoEventKind};

/// All the currently active subscriptions
#[derive(Debug)]
pub struct Subscriptions {
    /// Tracks the subscribers for each event identified by the key
    subs: HashMap<Cow<'static, str>, Vec<EventSubscription>>,
}

impl Subscriptions {
    /// Register a subscription for a method
    pub fn add_listener(&mut self, req: SubscriptionRequest) {
        let SubscriptionRequest {
            listener,
            method,
            kind,
        } = req;
        let subs = self.subs.entry(method).or_insert_with(Vec::new);
        subs.push(EventSubscription {
            listener,
            kind,
            queued_events: Default::default(),
        });
    }

    pub fn start_send<T: Event>(&mut self, method: &str, event: T) {
        if let Some(subscriptions) = self.subs.get_mut(method) {
            let event: Arc<dyn Event> = Arc::new(event);
            subscriptions
                .iter_mut()
                .for_each(|sub| sub.start_send(Arc::clone(&event)));
        }
    }

    pub fn try_send_custom(
        &mut self,
        method: &str,
        val: serde_json::Value,
    ) -> serde_json::Result<()> {
        if let Some(subscriptions) = self.subs.get_mut(method) {
            let mut event = None;
            if let Some(json_to_arc_event) = subscriptions
                .iter()
                .filter_map(|sub| {
                    if let EventKind::Custom(conv) = &sub.kind {
                        Some(conv)
                    } else {
                        None
                    }
                })
                .next()
            {
                event = Some(json_to_arc_event(val)?);
            }
            if let Some(event) = event {
                subscriptions
                    .iter_mut()
                    .filter(|sub| sub.kind.is_custom())
                    .for_each(|sub| sub.start_send(Arc::clone(&event)));
            }
        }
        Ok(())
    }

    /// Drains all queued events and does the housekeeping when the receiver
    /// part of a subscription is dropped
    pub fn poll(&mut self, cx: &mut Context<'_>) {
        for subscriptions in self.subs.values_mut() {
            for n in (0..subscriptions.len()).rev() {
                let mut sub = subscriptions.swap_remove(n);
                match sub.poll(cx) {
                    Poll::Ready(Err(err)) => {
                        if !err.is_disconnected() {
                            subscriptions.push(sub)
                        }
                    }
                    _ => subscriptions.push(sub),
                }
            }
        }
    }
}

pub struct SubscriptionRequest {
    listener: UnboundedSender<Arc<dyn Event>>,
    method: Cow<'static, str>,
    kind: EventKind,
}

impl fmt::Debug for SubscriptionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventSubscription")
            .field("method", &self.method)
            .field("kind", &self.kind)
            .finish()
    }
}

/// Represents a single event listener
pub struct EventSubscription {
    /// the sender half of the event channel
    listener: UnboundedSender<Arc<dyn Event>>,
    /// currently queued events
    queued_events: VecDeque<Arc<dyn Event>>,
    /// For what kind of event this event is for
    kind: EventKind,
}

impl EventSubscription {
    /// queue in a new event
    pub fn start_send(&mut self, event: Arc<dyn Event>) {
        self.queued_events.push_back(event)
    }

    /// Drains all queued events and begins the process of sending them to the
    /// sink.
    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), SendError>> {
        loop {
            match Sink::poll_ready(Pin::new(&mut self.listener), cx) {
                Poll::Ready(Ok(_)) => {}
                Poll::Ready(Err(err)) => {
                    // disconnected
                    return Poll::Ready(Err(err));
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
            if let Some(event) = self.queued_events.pop_front() {
                if let Err(err) = Sink::start_send(Pin::new(&mut self.listener), event) {
                    return Poll::Ready(Err(err));
                }
            } else {
                return Poll::Ready(Ok(()));
            }
        }
    }
}

impl fmt::Debug for EventSubscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventSubscription").finish()
    }
}

/// The receiver part of an event subscription
pub struct EventStream<T: IntoEventKind> {
    events: UnboundedReceiver<Arc<dyn Event>>,
    _marker: PhantomData<T>,
}

impl<T: IntoEventKind> fmt::Debug for EventStream<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStream").finish()
    }
}

impl<T: IntoEventKind> EventStream<T> {
    pub fn new(events: UnboundedReceiver<Arc<dyn Event>>) -> Self {
        Self {
            events,
            _marker: PhantomData,
        }
    }
}

impl<T: IntoEventKind + Unpin> Stream for EventStream<T> {
    type Item = Arc<T>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pin = self.get_mut();
        match Stream::poll_next(Pin::new(&mut pin.events), cx) {
            Poll::Ready(Some(event)) => {
                if let Ok(e) = event.into_any_arc().downcast() {
                    Poll::Ready(Some(e))
                } else {
                    Poll::Pending
                }
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chromiumoxide_cdp::cdp::CustomEvent;
    use chromiumoxide_types::Method;
    use futures::{SinkExt, StreamExt};

    #[async_std::test]
    async fn event_stream() {
        use chromiumoxide_cdp::cdp::browser_protocol::animation::EventAnimationCanceled;

        let (mut tx, rx) = futures::channel::mpsc::unbounded();
        let mut stream = EventStream::<EventAnimationCanceled>::new(rx);

        let event = EventAnimationCanceled {
            id: "id".to_string(),
        };
        let msg: Arc<dyn Event> = Arc::new(event.clone());
        tx.send(msg).await.unwrap();
        let next = stream.next().await.unwrap();
        assert_eq!(&*next, &event);
    }

    #[async_std::test]
    async fn custom_event_stream() {
        use serde::Deserialize;

        #[derive(Debug, Clone, Eq, PartialEq, Deserialize)]
        struct MyCustomEvent {
            name: String,
        }

        impl Method for MyCustomEvent {
            fn identifier(&self) -> Cow<'static, str> {
                "Custom.Event".into()
            }
        }
        impl CustomEvent for MyCustomEvent {}

        let (mut tx, rx) = futures::channel::mpsc::unbounded();
        let mut stream = EventStream::<MyCustomEvent>::new(rx);

        let event = MyCustomEvent {
            name: "my event".to_string(),
        };
        let msg: Arc<dyn Event> = Arc::new(event.clone());
        tx.send(msg).await.unwrap();
        let next = stream.next().await.unwrap();
        assert_eq!(&*next, &event);
    }
}
