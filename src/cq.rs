// Copyright 2017 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.


use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use grpc_sys::{self, GprClockType, GrpcCompletionQueue};
use futures::Async;
use futures::future::BoxFuture;
use futures::executor::{Notify, Spawn};
use crossbeam::sync::SegQueue;

use async::{SpinLock, Alarm, CallTag};
use util;

pub use grpc_sys::GrpcCompletionType as EventType;
pub use grpc_sys::GrpcEvent as Event;

/// `CompletionQueueHandle` enable notification of the completion of asynchronous actions.
pub struct CompletionQueueHandle {
    cq: *mut GrpcCompletionQueue,
}

unsafe impl Sync for CompletionQueueHandle {}
unsafe impl Send for CompletionQueueHandle {}

impl CompletionQueueHandle {
    pub fn new() -> CompletionQueueHandle {
        CompletionQueueHandle {
            cq: unsafe { grpc_sys::grpc_completion_queue_create_for_next(ptr::null_mut()) },
        }
    }
}

impl Drop for CompletionQueueHandle {
    fn drop(&mut self) {
        unsafe { grpc_sys::grpc_completion_queue_destroy(self.cq) }
    }
}

#[derive(Clone)]
pub struct CompletionQueue {
    handle: Arc<CompletionQueueHandle>,
    id: usize,
    fq: Arc<ReadyQueue>,
}

impl CompletionQueue {
    pub fn new(handle: Arc<CompletionQueueHandle>, id: usize) -> CompletionQueue {
        let fq = ReadyQueue {
            queue: SegQueue::new(),
            pending: AtomicUsize::new(0),
            alarm: SpinLock::new(None),
            worker_id: id,
        };
        CompletionQueue {
            handle: handle,
            id: id,
            fq: Arc::new(fq),
        }
    }

    /// Blocks until an event is available, the completion queue is being shut down.
    pub fn next(&self) -> Event {
        unsafe {
            let inf = grpc_sys::gpr_inf_future(GprClockType::Realtime);
            grpc_sys::grpc_completion_queue_next(self.handle.cq, inf, ptr::null_mut())
        }
    }

    /// Begin destruction of a completion queue.
    ///
    /// Once all possible events are drained then `next()` will start to produce
    /// `Event::QueueShutdown` events only.
    pub fn shutdown(&self) {
        unsafe {
            grpc_sys::grpc_completion_queue_shutdown(self.handle.cq);
        }
    }

    pub fn as_ptr(&self) -> *mut GrpcCompletionQueue {
        self.handle.cq
    }

    pub fn worker_id(&self) -> usize {
        self.id
    }

    fn push_and_notify(&self, f: Item) {
        self.fq.push_and_notify(f, self.clone())
    }

    fn pop_and_poll(&self) {
        self.fq.pop_and_poll(self.clone());
    }
}

type Item = Spawn<BoxFuture<(), ()>>;

struct ReadyQueue {
    queue: SegQueue<Item>,
    pending: AtomicUsize,
    alarm: SpinLock<Option<Alarm>>,
    worker_id: usize,
}

impl ReadyQueue {
    fn push_and_notify(&self, f: Item, cq: CompletionQueue) {
        let notify = QueueNotify::new(cq.clone());

        if util::get_worker_id() == self.worker_id {
            let notify = Arc::new(notify);
            poll(f, &notify);
        } else {
            self.queue.push(f);
            let pending = self.pending.fetch_add(1, Ordering::SeqCst);
            if 0 == pending {
                let tag = Box::new(CallTag::Queue(notify));
                let mut alarm = self.alarm.lock();
                // We need to keep the alarm until queue is empty.
                *alarm = Some(Alarm::new(&cq, tag));
                alarm.as_mut().unwrap().alarm();
            }
        }
    }

    fn pop_and_poll(&self, cq: CompletionQueue) {
        let mut notify = Arc::new(QueueNotify::new(cq.clone()));
        let mut done = true;

        while 0 != self.pending.fetch_sub(1, Ordering::SeqCst) {
            notify = if done {
                // Future has resloved, and the notify is empty, reuse it.
                notify
            } else {
                // Future is not complete yet. Other thread holds the notify,
                // create a new one for the next ready Future.
                Arc::new(QueueNotify::new(cq.clone()))
            };

            if let Some(f) = self.queue.try_pop() {
                done = poll(f, &notify);
            }
        }
        self.alarm.lock().take().expect("must have an Alarm");
    }
}

fn poll(f: Item, notify: &Arc<QueueNotify>) -> bool {
    let mut option = notify.f.lock();
    *option = Some(f);
    match option.as_mut().unwrap().poll_future_notify(notify, 0) {
        Err(_) |
        Ok(Async::Ready(_)) => {
            // Future has resloved, empty the future so that we can
            // reuse the notify.
            option.take();
            true
        }
        Ok(Async::NotReady) => {
            // Future is not complete yet.
            false
        }
    }
}

#[derive(Clone)]
pub struct QueueNotify {
    cq: CompletionQueue,
    f: Arc<SpinLock<Option<Item>>>,
}

unsafe impl Send for QueueNotify {}
unsafe impl Sync for QueueNotify {}

impl QueueNotify {
    pub fn new(cq: CompletionQueue) -> QueueNotify {
        QueueNotify {
            cq: cq,
            f: Arc::new(SpinLock::new(None)),
        }
    }

    pub fn resolve(self, success: bool) {
        // it should always be canceled for now.
        assert!(!success);
        self.cq.pop_and_poll();
    }

    pub fn push_and_notify(&self, f: Item) {
        self.cq.push_and_notify(f);
    }
}

impl Notify for QueueNotify {
    fn notify(&self, _: usize) {
        if let Some(f) = self.f.lock().take() {
            self.cq.push_and_notify(f);
        }
    }
}
