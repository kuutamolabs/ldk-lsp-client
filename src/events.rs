// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Events are surfaced by the library to indicate some action must be taken
//! by the end-user.
//!
//! Because we don't have a built-in runtime, it's up to the end-user to poll
//! [`LiquidityManager::get_and_clear_pending_events`] to receive events.
//!
//! [`LiquidityManager::get_and_clear_pending_events`]: crate::LiquidityManager::get_and_clear_pending_events

use crate::lsps0;
#[cfg(lsps1)]
use crate::lsps1;
use crate::lsps2;
use crate::prelude::{Vec, VecDeque};
use crate::sync::{Arc, Mutex};

use core::future::Future;
use core::task::{Poll, Waker};

pub(crate) struct EventQueue {
	queue: Arc<Mutex<VecDeque<Event>>>,
	waker: Arc<Mutex<Option<Waker>>>,
	#[cfg(feature = "std")]
	condvar: std::sync::Condvar,
}

impl EventQueue {
	pub fn new() -> Self {
		let queue = Arc::new(Mutex::new(VecDeque::new()));
		let waker = Arc::new(Mutex::new(None));
		#[cfg(feature = "std")]
		{
			let condvar = std::sync::Condvar::new();
			Self { queue, waker, condvar }
		}
		#[cfg(not(feature = "std"))]
		Self { queue, waker }
	}

	pub fn enqueue(&self, event: Event) {
		{
			let mut queue = self.queue.lock().unwrap();
			queue.push_back(event);
		}

		if let Some(waker) = self.waker.lock().unwrap().take() {
			waker.wake();
		}
		#[cfg(feature = "std")]
		self.condvar.notify_one();
	}

	pub fn next_event(&self) -> Option<Event> {
		self.queue.lock().unwrap().pop_front()
	}

	pub async fn next_event_async(&self) -> Event {
		EventFuture { event_queue: Arc::clone(&self.queue), waker: Arc::clone(&self.waker) }.await
	}

	#[cfg(feature = "std")]
	pub fn wait_next_event(&self) -> Event {
		let mut queue =
			self.condvar.wait_while(self.queue.lock().unwrap(), |queue| queue.is_empty()).unwrap();

		let event = queue.pop_front().expect("non-empty queue");
		let should_notify = !queue.is_empty();

		drop(queue);

		if should_notify {
			if let Some(waker) = self.waker.lock().unwrap().take() {
				waker.wake();
			}

			self.condvar.notify_one();
		}

		event
	}

	pub fn get_and_clear_pending_events(&self) -> Vec<Event> {
		self.queue.lock().unwrap().drain(..).collect()
	}
}

/// An event which you should probably take some action in response to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
	/// An LSPS0 client event.
	LSPS0Client(lsps0::event::LSPS0ClientEvent),
	/// An LSPS1 (Channel Request) client event.
	#[cfg(lsps1)]
	LSPS1Client(lsps1::event::LSPS1ClientEvent),
	/// An LSPS1 (Channel Request) server event.
	#[cfg(lsps1)]
	LSPS1Service(lsps1::event::LSPS1ServiceEvent),
	/// An LSPS2 (JIT Channel) client event.
	LSPS2Client(lsps2::event::LSPS2ClientEvent),
	/// An LSPS2 (JIT Channel) server event.
	LSPS2Service(lsps2::event::LSPS2ServiceEvent),
}

struct EventFuture {
	event_queue: Arc<Mutex<VecDeque<Event>>>,
	waker: Arc<Mutex<Option<Waker>>>,
}

impl Future for EventFuture {
	type Output = Event;

	fn poll(
		self: core::pin::Pin<&mut Self>, cx: &mut core::task::Context<'_>,
	) -> core::task::Poll<Self::Output> {
		if let Some(event) = self.event_queue.lock().unwrap().pop_front() {
			Poll::Ready(event)
		} else {
			*self.waker.lock().unwrap() = Some(cx.waker().clone());
			Poll::Pending
		}
	}
}
