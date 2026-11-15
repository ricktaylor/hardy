// The bounded concurrent drain behind the egress (peer) and delivery
// (service) seats. Queues hold waiting work; lanes carry in-flight work
// (docs/policy_subsystem_redesign.md): a seat drains its queues into at most
// its effective lane count of concurrent hand-offs — the resource's declared
// lanes bounded by the BPA's own seat cap, because every in-flight item holds
// a loaded bundle. The lanes are a budget of permits shared by every queue
// of the seat (a peer's policy queues), so the declaration holds whatever
// the number of queues.

use core::{
    future::Future,
    num::{NonZeroU32, NonZeroUsize},
    pin::pin,
};

use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use hardy_async::{OwnedSemaphorePermit, Semaphore, closeable::Receiver};

use super::Lanes;

// A seat's effective lane count: one for a serial resource, a limited one's
// count bounded by the seat cap, and the cap for an unlimited one.
pub fn effective(lanes: Lanes, cap: NonZeroU32) -> NonZeroU32 {
    match lanes {
        Lanes::Serial => NonZeroU32::MIN,
        Lanes::Limited(n) => n.min(cap),
        Lanes::Unlimited => cap,
    }
}

// A seat's lane budget: `lanes` permits, one per in-flight hand-off. Its
// clones share the budget.
pub fn budget(lanes: NonZeroU32) -> Semaphore {
    Semaphore::new(NonZeroUsize::try_from(lanes).unwrap_or(NonZeroUsize::MAX))
}

// Drain `rx` into hand-offs, each holding a lane of the seat's `lanes`
// budget. A lane is taken before the next item is pulled, so while every
// lane is busy the queue is left alone: its hybrid channel fills and spills
// to storage exactly as it does behind a serial consumer, and a budget of
// one lane is that serial consumer. A queue found empty hands its lane back
// while it waits for an item, so an idle queue holds none of a shared
// budget; with other queues drawing on the same budget, the item that
// arrives may then wait in hand for a lane — at most one per queue. Items
// start in queue order and may complete in any order.
//
// `hand_off` starts an item's hand-off with the lane's permit, which it
// must hold until the hand-off ends, and returns its completion, which must
// make progress without being polled (a spawned task's handle): the drain
// polls completions only to collect them. Once the queue has closed and
// drained, the drain returns only after every in-flight hand-off has
// completed.
pub async fn drain<T, F, H>(rx: &Receiver<T>, lanes: &Semaphore, mut hand_off: F)
where
    F: FnMut(T, OwnedSemaphorePermit) -> H,
    H: Future,
{
    let mut in_flight = FuturesUnordered::new();
    loop {
        let mut lane = collecting(&mut in_flight, lanes.acquire_owned()).await;
        if rx.is_empty() {
            drop(lane);
            let Ok(item) = collecting(&mut in_flight, rx.recv()).await else {
                break;
            };
            lane = collecting(&mut in_flight, lanes.acquire_owned()).await;
            in_flight.push(hand_off(item, lane));
        } else {
            // The queue's only consumer saw an item buffered, so this
            // returns it at once.
            let Ok(item) = rx.recv().await else {
                break;
            };
            in_flight.push(hand_off(item, lane));
        }
    }
    while in_flight.next().await.is_some() {}
}

// Await `fut`, collecting finished hand-offs meanwhile, so completions are
// observed (and, for unspawned ones, driven) while the drain waits.
async fn collecting<H, O>(in_flight: &mut FuturesUnordered<H>, fut: impl Future<Output = O>) -> O
where
    H: Future,
{
    let mut fut = pin!(fut.fuse());
    loop {
        if in_flight.is_empty() {
            return fut.await;
        }
        futures::select_biased! {
            _ = in_flight.select_next_some() => {}
            out = fut => return out,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::{future::BoxFuture, pin_mut, poll};
    use hardy_async::closeable;

    use super::*;

    // The hand-offs a drain has started, in start order, each completed
    // when the test drops its sender; a hand-off holds its lane until it
    // completes. flume stays qualified: its channel types share names with
    // the `closeable` ones the drain takes.
    #[derive(Default)]
    struct Started(Mutex<Vec<(u32, Option<flume::Sender<()>>)>>);

    impl Started {
        fn hand_off(&self, item: u32, lane: OwnedSemaphorePermit) -> BoxFuture<'static, ()> {
            let (done_tx, done_rx) = flume::bounded::<()>(1);
            self.0.lock().unwrap().push((item, Some(done_tx)));
            async move {
                let _ = done_rx.recv_async().await;
                drop(lane);
            }
            .boxed()
        }

        fn items(&self) -> Vec<u32> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|(item, _)| *item)
                .collect()
        }

        fn complete(&self, item: u32) {
            let mut started = self.0.lock().unwrap();
            let (_, done) = started
                .iter_mut()
                .find(|(i, _)| *i == item)
                .expect("the item was started");
            drop(done.take().expect("the item was completed once"));
        }
    }

    async fn queue_of(
        items: impl IntoIterator<Item = u32>,
    ) -> (closeable::Sender<u32>, closeable::Receiver<u32>) {
        let (tx, rx) = closeable::bounded(16);
        for item in items {
            tx.send(item).await.unwrap();
        }
        (tx, rx)
    }

    fn lanes(n: u32) -> Semaphore {
        budget(NonZeroU32::new(n).unwrap())
    }

    // A serial resource gets one lane whatever the cap, a limited one its
    // count bounded by the cap, and an unlimited one the cap.
    #[test]
    fn effective_lanes_follow_the_declaration_within_the_cap() {
        let n = |n| NonZeroU32::new(n).unwrap();
        assert_eq!(effective(Lanes::Serial, n(8)), n(1));
        assert_eq!(effective(Lanes::Limited(n(3)), n(8)), n(3));
        assert_eq!(effective(Lanes::Limited(n(12)), n(8)), n(8));
        assert_eq!(effective(Lanes::Unlimited, n(8)), n(8));
    }

    // The drain starts exactly `lanes` hand-offs in queue order, leaves the
    // rest queued, and pulls exactly one more each time a lane frees —
    // whichever lane it is.
    #[tokio::test]
    async fn the_drain_fills_its_lanes_and_pulls_as_each_frees() {
        let (_tx, rx) = queue_of(0..5).await;
        let started = Started::default();
        let lanes = lanes(2);
        let drain = drain(&rx, &lanes, |item, lane| started.hand_off(item, lane));
        pin_mut!(drain);

        assert!(poll!(drain.as_mut()).is_pending());
        assert_eq!(started.items(), [0, 1]);
        assert_eq!(
            rx.len(),
            3,
            "the queue is left alone while every lane is busy"
        );

        // Completions may reorder: the second lane frees first.
        started.complete(1);
        assert!(poll!(drain.as_mut()).is_pending());
        assert_eq!(started.items(), [0, 1, 2]);
        assert_eq!(rx.len(), 2);

        started.complete(0);
        assert!(poll!(drain.as_mut()).is_pending());
        assert_eq!(started.items(), [0, 1, 2, 3]);
        assert_eq!(rx.len(), 1);
    }

    // One lane is a serial consumer: each item is pulled only once the
    // previous hand-off has completed.
    #[tokio::test]
    async fn one_lane_drains_serially_in_queue_order() {
        let (_tx, rx) = queue_of(0..3).await;
        let started = Started::default();
        let lanes = lanes(1);
        let drain = drain(&rx, &lanes, |item, lane| started.hand_off(item, lane));
        pin_mut!(drain);

        for item in 0..3 {
            assert!(poll!(drain.as_mut()).is_pending());
            assert_eq!(started.items(), (0..=item).collect::<Vec<_>>());
            assert_eq!(rx.len(), (2 - item) as usize);
            started.complete(item);
        }
    }

    // Closing the queue lets the drain take what is still buffered, and it
    // returns only once every in-flight hand-off has completed.
    #[tokio::test]
    async fn a_closed_queue_drains_and_waits_for_its_lanes() {
        let (tx, rx) = queue_of(0..3).await;
        let started = Started::default();
        let lanes = lanes(2);
        let drain = drain(&rx, &lanes, |item, lane| started.hand_off(item, lane));
        pin_mut!(drain);

        assert!(poll!(drain.as_mut()).is_pending());
        tx.close();
        started.complete(0);
        assert!(poll!(drain.as_mut()).is_pending());
        assert_eq!(started.items(), [0, 1, 2], "the buffered item still drains");

        started.complete(2);
        assert!(
            poll!(drain.as_mut()).is_pending(),
            "a hand-off is still in flight"
        );
        started.complete(1);
        assert!(poll!(drain.as_mut()).is_ready());
    }

    // The queues of one seat share its lanes: two busy queues together keep
    // only the budget's hand-offs in flight, and freed lanes go to waiting
    // queues in the order they began waiting, so the queues alternate.
    #[tokio::test]
    async fn queues_sharing_a_budget_share_its_lanes() {
        let (_tx_a, rx_a) = queue_of(0..3).await;
        let (_tx_b, rx_b) = queue_of(10..13).await;
        let started = Started::default();
        let lanes = lanes(2);
        let drain_a = drain(&rx_a, &lanes, |item, lane| started.hand_off(item, lane));
        let drain_b = drain(&rx_b, &lanes, |item, lane| started.hand_off(item, lane));
        pin_mut!(drain_a, drain_b);

        assert!(poll!(drain_a.as_mut()).is_pending());
        assert!(poll!(drain_b.as_mut()).is_pending());
        assert_eq!(started.items(), [0, 1], "the first queue took both lanes");
        assert_eq!(rx_b.len(), 3, "the second queue pulled nothing");

        // The first freed lane goes to the first queue, which began waiting
        // first; the next to the second queue.
        started.complete(0);
        assert!(poll!(drain_a.as_mut()).is_pending());
        assert!(poll!(drain_b.as_mut()).is_pending());
        assert_eq!(started.items(), [0, 1, 2]);
        started.complete(1);
        assert!(poll!(drain_a.as_mut()).is_pending());
        assert!(poll!(drain_b.as_mut()).is_pending());
        assert_eq!(started.items(), [0, 1, 2, 10]);
        assert_eq!((rx_a.len(), rx_b.len()), (0, 2));
    }

    // An idle queue holds none of a shared budget: with one lane, a queue
    // waiting on an empty channel leaves the lane to a busy queue, and an
    // item that then arrives waits in hand for its turn at the lane.
    #[tokio::test]
    async fn an_idle_queue_leaves_the_shared_lanes_to_busy_ones() {
        let (tx_idle, rx_idle) = queue_of([]).await;
        let (_tx_busy, rx_busy) = queue_of(10..13).await;
        let started = Started::default();
        let lanes = lanes(1);
        let idle = drain(&rx_idle, &lanes, |item, lane| started.hand_off(item, lane));
        let busy = drain(&rx_busy, &lanes, |item, lane| started.hand_off(item, lane));
        pin_mut!(idle, busy);

        assert!(poll!(idle.as_mut()).is_pending());
        assert!(poll!(busy.as_mut()).is_pending());
        assert_eq!(started.items(), [10], "the busy queue has the lane");

        tx_idle.send(0).await.unwrap();
        assert!(poll!(idle.as_mut()).is_pending());
        assert_eq!(started.items(), [10], "the arrival waits for the lane");
        assert!(rx_idle.is_empty(), "it was pulled, and is held in hand");

        // The busy queue was already waiting, so it takes the freed lane
        // first; the next freed lane is the arrival's.
        started.complete(10);
        assert!(poll!(busy.as_mut()).is_pending());
        assert!(poll!(idle.as_mut()).is_pending());
        assert_eq!(started.items(), [10, 11]);
        started.complete(11);
        assert!(poll!(busy.as_mut()).is_pending());
        assert!(poll!(idle.as_mut()).is_pending());
        assert_eq!(started.items(), [10, 11, 0]);
    }
}
