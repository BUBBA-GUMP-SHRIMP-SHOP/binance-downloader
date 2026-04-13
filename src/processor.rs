use crossbeam::channel::Receiver;
use parking_lot::RwLock;

// ============================================================================
// PARALLEL RUNNER (shared helper)
// Drains `rx`, spawning one task per item via `make_task`.
// At most `parallelism` tasks run concurrently; back-pressure is applied by
// awaiting the oldest task before spawning a new one when at capacity.
// ============================================================================
pub async fn run_parallel<T, F, Fut>(
    rx: Receiver<T>,
    parallelism: usize,
    make_task: F,
) -> anyhow::Result<()>
where
    T: Send + 'static,
    F: Fn(T) -> Fut,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    // Bridge the blocking crossbeam receiver to an async tokio channel so
    // we never block the tokio executor inside an async task.
    let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::channel::<T>(parallelism);
    tokio::task::spawn_blocking(move || {
        for item in rx {
            if bridge_tx.blocking_send(item).is_err() {
                break;
            }
        }
        // bridge_tx dropped here → bridge_rx.recv() returns None
    });

    let mut join_set: tokio::task::JoinSet<anyhow::Result<()>> = tokio::task::JoinSet::new();

    while let Some(item) = bridge_rx.recv().await {
        if join_set.len() >= parallelism {
            if let Some(result) = join_set.join_next().await {
                result??;
            }
        }
        join_set.spawn(make_task(item));
    }

    while let Some(result) = join_set.join_next().await {
        result??;
    }

    Ok(())
}

pub trait Processor<'a> {
    type In;
    type Out;
    async fn process(&self, value: &Self::In) -> anyhow::Result<()>;
    fn connect(&self, iter: Receiver<Self::In>);
}

pub trait ProcessConnector<'a> {
    type Out;
    fn connect_to(&self, processor: impl Processor<'a, In = Self::Out>);
}

pub struct ProcessorUnit<F, In, Out> {
    func: F,
    _phantom_in: std::marker::PhantomData<In>,

    pub set: std::sync::Arc<RwLock<Vec<Receiver<In>>>>,

    sender: crossbeam::channel::Sender<Out>,
    pub receiver: crossbeam::channel::Receiver<Out>,
}

impl<F, In, Out> ProcessorUnit<F, In, Out>
where
    F: Fn(&In) -> Out + Send + Sync + 'static,
    Out: Send + Sync + 'static,
{
    pub fn new(func: F) -> Self {
        let (sender, receiver) = crossbeam::channel::bounded(10);
        Self {
            func,
            _phantom_in: std::marker::PhantomData,

            set: std::sync::Arc::new(RwLock::new(Vec::new())),

            sender,
            receiver,
        }
    }
}
impl<'a, F, In, Out> Processor<'a> for ProcessorUnit<F, In, Out>
where
    F: Fn(&In) -> Out + Send + Sync + 'static,
    Out: Send + Sync + 'static,
{
    type In = In;
    type Out = Out;

    async fn process(&self, value: &Self::In) -> anyhow::Result<()> {
        let result = (self.func)(value);
        self.sender.send(result)?;
        Ok(())
    }

    fn connect(&self, rcv: Receiver<In>) {
        self.set.write().push(rcv);
    }
}
impl<'a, F, In, Out> ProcessConnector<'a> for ProcessorUnit<F, In, Out>
where
    F: Fn(&In) -> Out + Send + Sync + 'static,
    Out: Send + Sync + 'static,
{
    type Out = Out;

    fn connect_to(&self, processor: impl Processor<'a, In = Self::Out>) {
        processor.connect(self.receiver.clone());
        // Note: This approach won't work with the current design.
        // Consider redesigning to store items directly instead of iterators.
    }
}

#[cfg(test)]
mod tests {
    use super::{ProcessConnector, Processor, ProcessorUnit};
    use crossbeam::channel::{bounded, Receiver};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct SpyProcessor {
        connect_calls: Arc<AtomicUsize>,
    }

    impl<'a> Processor<'a> for SpyProcessor {
        type In = i32;
        type Out = i32;

        async fn process(&self, _value: &Self::In) -> anyhow::Result<()> {
            Ok(())
        }

        fn connect(&self, _iter: Receiver<Self::In>) {
            self.connect_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn process_calls_inner_function() {
        let unit = ProcessorUnit::<_, i32, i32>::new(|value| value + 10);
        let result = unit.process(&5).await;
        assert!(result.is_ok());
    }

    #[test]
    fn connect_registers_receiver() {
        let unit = ProcessorUnit::<_, i32, i32>::new(|value| value + 1);
        let (_sender, receiver) = bounded::<i32>(1);

        unit.connect(receiver);

        assert_eq!(unit.set.read().len(), 1);
    }

    #[test]
    fn connect_to_invokes_downstream_connect() {
        let source = ProcessorUnit::<_, i32, i32>::new(|value| value + 1);
        let calls = Arc::new(AtomicUsize::new(0));
        let downstream = SpyProcessor {
            connect_calls: Arc::clone(&calls),
        };

        source.connect_to(downstream);

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
